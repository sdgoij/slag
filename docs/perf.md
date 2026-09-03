# Performance

Current performance state of the slag runtime and the PLAN Phase 18
performance milestones, each behind a benchmark gate rather than a
correctness gate.

## Current architecture

- **Value representation**: a NaN-boxed `u64` (PLAN Phase 18): a quiet-NaN
  tag region (top 16 bits `0x7FF8`) holds a 4-bit tag plus a 44-bit payload
  for the heap variants; every other bit pattern is a double stored
  exactly. The payload holds the allocation pointer shifted right 4 (the
  box base is 16-byte aligned), so a full 48-bit address space round-trips.
  `Value` is `Copy` since the GC milestone (GC-5): the heap is
  GC-managed, so a value is a plain word with no refcount bookkeeping, and
  the collector traces the boxes. `match value.kind()` keeps the old enum
  arm shapes via a `ValueKind` mirror.
- **Interpreter**: a `Step` bytecode VM over the compiled function IR
  (`crates/runtime/src/ir.rs`): every expression and statement compiles to
  a `Step` at creation, and a `Vm` dispatch loop executes the compiled
  body for ordinary calls/constructs, generators, async functions, and
  top-level scripts. The old tree-walker survives only as isolated
  single-expression helpers (computed keys, destructuring defaults, class
  heritage); no statement or control-flow structure is walked anymore.
- **Objects**: ordinary Rust structs with a property vector and a lock-free
  prototype `Cell`; per-object state (promises, generators, buffers, ...)
  lives in agent-side tables keyed by object id. A shape/IC layer
  accelerates property access — map-based shapes, the generation-validated
  member-value cells, and a lazy key→slot hash index for large property
  vectors (all invalidated by structural changes only).
- **Strings**: UTF-16 code units in a flat buffer or a depth-capped rope of
  concatenation nodes (see the string-rope milestone below); `concat` appends
  in O(1) once a string is large, and the flat form is materialized lazily
  and cached. Strings of ≤16 units live inline in the box (Cut 67).
- **Memory**: a GC-managed arena heap — bump allocation + mark-sweep with
  root tracing (incl. a conservative native-stack scan), ephemeron-aware
  WeakMap/WeakSet, and `WeakRef`/`FinalizationRegistry` driven by the
  heap; `--gc-stress` collects per allocation. See `docs/gc-plan.md`.

## Benchmark gate

The CLI's `--bench` mode runs a fixed micro-benchmark suite and reports
wall time per benchmark:

```
cargo run -p cli --release -- --bench
```

Current benchmarks (12 rows): arithmetic, bare loop, indexed store,
property access, string concatenation, array iteration, function calls,
closure capture, per-iteration, construct churn, and the two buildString
shapes. Each snippet is evaluated once to warm
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

### Current status (measured 2026-09-01)

All five benchmark-gate rows have closed their ≥5x target by one to two
orders of magnitude — the bytecode-VM, shapes/IC, rope, and NaN-boxing
milestones are all done (see the Deferred milestones table). Interpreter
medians (release, 3-run, `--bench`; the rows below are the original five):

| Benchmark | corrected baseline | today | speedup | 5x target |
|---|---|---|---|---|
| arithmetic | 2.52s | ~15.2ms | ~166x | 0.50s — met |
| property access | 3.22s | ~28.3ms | ~114x | 0.64s — met |
| string concat | 0.88s | ~4.0ms | ~218x | — |
| array iteration | 15.42s | ~25.0ms | ~617x | 3.08s — met |
| function calls | 5.73s | ~27.5ms | ~208x | 1.15s — met |

(The `bytecode-plan.md` gate used a later, already-optimized walker
baseline — arithmetic 1.14s → the plan's "≤0.23s"; the current 15.2ms is
~15x below that target too. The machine-drift caveat from the earlier
sections applies — judge only multi-run deltas.)

The full `--bench` suite (12 rows) and the newer interpreter rows: bare
loop ~14.4ms, indexed store ~48ms, closure capture ~33ms, per-iteration
~9.2ms, construct churn ~17ms, buildString shape ~180ms (all 1M-iteration
loops except buildString's 3M). The per-op floor behind these is ~50x
lower than the 2026-08-31 measurement — see the floor section below. The
remaining levers are listed in the levers tables in the typed-array and
floor sections.

### Node comparison and optimization plan (measured 2026-08-21)

Fresh baseline (release, current tree, 3-run medians of `--bench`) with the
same sources run through node v24.12.0 (warm, 3-run medians):

| Benchmark | slag | node | ratio |
|---|---|---|---|
| arithmetic | 1.058s | 8.43ms | 125x |
| property access | 1.318s | 1.31ms | 1006x |
| string concat | 0.150s | 1.43ms | 105x |
| array iteration | 13.25s | 59.4ms | 223x |
| function calls | 1.829s | 1.33ms | 1375x |

(The ratios are JIT-dominated — V8 inlines/constant-folds these shapes —
so they rank the machinery-heavy paths rather than set a target; the gate
remains the internal ≥5x.)

Probe decomposition (release, interleaved 3-run medians) shows where the
time goes:

- **Array iteration is 75% of the bench suite** (13.25s of ~17.6s). An
  empty for-of body costs 12.98s — ~11s (83%) is the iterator machinery
  (iterator object + 1M `next()` calls + 1M result-object allocations);
  the same workload with an index loop is 2.00s (~6x headroom).
- **Arithmetic is bound by the global path**: top-level `n += i * 2` is
  1.05s vs 0.135s for the same loop in a fast-certified function (frame
  slots); the empty top-level loop is ~0.57s. A global-var cell (cached
  global-object slot per name) is the documented lever.
- **Function calls** (1.83s) pay the full `ExecutionContext` push/pop and
  ~10 Rc refcount bumps per 1M calls.

Ranked plan (each behind the usual zero-regression + clippy validation):

1. **P0 — dense-array for-of fast path** — *landed*: a plain Array with the
   stock `@@iterator` iterates by index (new `expr::for_of_begin` guard +
   `ForOfEntry::Fast` in the Vm), re-reading `length` and each element per
   step so a body that mutates the array is observed exactly as the stock
   iterator would; any shadowed/patched `@@iterator`/`next`/`return`, a
   proxy, or a non-Array receiver falls back to the generic protocol. Array
   iteration measured **13.25s → 2.70s (~4.9x)**; full sweeps at zero
   regressions (language 23,690/0/34 unchanged; built-ins 23,432/0/154 —
   eight marginal RegExp property-escape fixtures stopped timing out).
2. **P1 — global var cells** — *landed*: the `Vm` caches the global
   object's property-vector slot per declared top-level name (`global_cells`
   + `load_global_value`/`store_global_value`), re-validated on every access
   against the stored key — an insert/delete/redefinition shifts or
   replaces slots, and the reference path re-resolves (strict non-writable
   errors, accessors, and missing bindings all fall through). Measured:
   arithmetic 1.06s → **0.19s**, property access 1.32s → **0.35s**, string
   concat 0.15s → **0.08s**, function calls 1.83s → **0.69s** (array
   iteration unchanged at 2.7s); full sweeps at zero regressions
   (language 23,690/0/34 unchanged; built-ins 23,436/0/154 — four more
   marginal RegExp fixtures stopped timing out).
3. **P2 — lightweight call path** — *landed*: the certified fast body's
   pushed `ExecutionContext` drops the `script_or_module`/`source`/
   `private_environment` clones (its certification excludes the only
   readers — eval/import/private/Annex B/function-creation), the record is
   fetched with one scoped borrow per branch (a `.cloned()` would
   deep-copy the whole record per call), the VM call steps call
   `function::call_inner` directly (the agent is already set for the
   duration of `run_inner`, so the two TLS swaps are redundant), and
   `dispose_env_resources` skips its drain via a cheap emptiness read.
   Measured: function calls 1.83s → **0.63s**; full sweeps at zero
   regressions (language 23,690/0/34 unchanged; built-ins 23,426/0/154 —
   the pass/hang delta vs the P1 run is the marginal RegExp property-escape
   timeout boundary, all new hangs in the known slow set).
4. **P3 — property-access IC** — *landed*: a small direct-mapped cache on
   the `Vm` (`member_cells`, 16 entries, `(object id, name atom) → slot`)
   serves `GetMemberName`/`GetMemberComputed` own-data reads — the slot is
   re-validated on every access against the stored key and property kind, so
   a structural change, redefinition, accessor conversion, or hash collision
   falls back to the full Get and re-resolves. Measured: property access
   1.32s → **0.24s**; full sweeps at zero regressions (language
   23,690/0/34 unchanged; built-ins 23,429/0/154, all hangs in the known
   slow set).
5. **Numeric-key array fast path** — *landed with P3*: a second direct-mapped
   cache (`array_element_cells`, `(object id, index) → slot`) serves
   `GetMemberComputed` for canonical Number indices and the dense-Array
   for-of fast path — a Number key converts purely (ToPropertyKey of a
   number runs no user code), so the element reads without the
   number→string→intern round-trip; the for-of fast path also inlines the
   length read (`array_length`, the array's `length` is always the first
   property-vector entry) and compiles a simple `var` head to
   `ForOfBindGlobal`/`ForOfBindLocal` instead of the per-element
   binding-initialization. Measured: array iteration 2.71s → **2.29s**, and
   the variable-index `a[j]` shape ~550ms → ~240ms; full sweeps at zero
   regressions (built-ins 23,438/0/154).

Methodology note: per-process timing is load-sensitive on this machine
(5.5x spurious swings observed), so only interleaved multi-run medians or
the in-process `--bench` harness count.

### Cut 11 — for-of begin hoist, fast-script certification, element writes (measured 2026-08-21)

Fresh 3-run medians of `--bench` (release), against the Node v24.12.0
comparison (same sources, warmed, 2nd run) both with and without the JIT:

| Benchmark | slag | node (jit) | node (--jitless) | slag vs jitless |
|---|---|---|---|---|
| arithmetic | 182ms | 1ms | 11ms | 17x |
| property access | 242ms | 1ms | 13ms | 19x |
| string concat | 78ms | 4ms | 5ms | 16x |
| array iteration | 324ms | 3ms | 26ms | 12x |
| function calls | 642ms | 1ms | 16ms | 40x |

The `--jitless` column (V8's ignition bytecode interpreter, no JIT) is the
realistic interpreter-vs-interpreter picture; the ratios rank the
remaining machinery: function calls are the standout gap (the per-call
`ExecutionContext` push), array iteration is the closest.

The ranked plan's five milestones all landed with zero conformance
regressions; four further wins followed (each with the same validation —
clippy clean, `cargo test --workspace` green, language sweep
23,690/0/34 unchanged, built-ins sweep 0 fail/0 crash):

6. **Hoisted for-of begin detection** — `for_of_begin` verifies the stock
   `%Array.prototype.values%` iterator over a plain Array (intrinsic
   identity of `@@iterator`/`next`, no `return` on the
   `%ArrayIteratorPrototype%` chain) *without allocating the iterator
   object or calling `values()`* — the created stock iterator is empty and
   unobservable, so the checks are observably identical. The begin was
   ~10µs (iterator allocation + `values()` call + array_iter_data
   bookkeeping + chain checks) and dominated the array bench (100k
   begins/bench): array iteration 2.17s → **1.37s**.
7. **Fast-script certification for `for-of` with a simple `var` head** —
   `script_scan_allows` rejected *every* script containing a `for-of`,
   sending the whole script (including its plain loops and the body's
   global reads) to the slow env-chain path (~7x slower loops). A `var`
   ident head binds via `ForOfBindGlobal`/`ForOfBindLocal` with no
   per-iteration environment, so the loop is fast-script-safe; lexical
   heads, destructuring heads, and env-path bodies still bail. Array
   iteration 1.37s → **0.33s** (the whole script finally runs on global
   cells); it also removed a per-process pollution where any prior for-of
   made later index loops 7x slower.
8. **O(1) property-index maintenance** — appends (the generic define and
   the array dense-append paths) now insert the new key into the lazy
   `property_index` HashMap instead of invalidating it, so sequential
   fills are no longer O(n²) (each member lookup on a growing array
   rebuilt the index). `Array.prototype.push` growth: 100k pushes >120s
   (quadratic) → **~1s** (linear); the property-escape fixtures' 10k+
   element builds stopped dominating their run time.
9. **O(1) IC slot resolution** — `resolve_member_cell` and
   `resolve_array_element` replaced linear `props.iter().position()` scans
   with the `property_index` (`JsObject::property_slot`), fixing the
   member-lookup half of the push quadratic and big-array element
   resolution (a 1M-element for-of that hung before completes in ~2s).
10. **Fast array element writes** — the compiled `a[i] = v` and
    `Array.prototype.push` element writes bypass the full `[[Set]]`
    machinery (`JsObject::array_element_write`): an existing own writable
    data element updates in place, and a missing element (hole fill or
    dense append) creates the own property after verifying a fully
    ordinary prototype chain with no own property at the index, a writable
    length, and an extensible array — anything else falls back to the full
    `[[Set]]` (accessors, proxies, frozen/sealed, strict-mode errors).
    Writes 3.1µs → **1.6µs** (dense), the property-escape build 4.3s →
    **2.9s** (and the push builtin from ~11µs — its dispatch-chain linear
    scan is a separate, still-open item).

Current suite total: **17.6s → ~1.47s (~12x)**; array iteration 13.25s →
324ms (~41x). All gates ≥5x vs the corrected baseline are met several
fold. The remaining known-slow conformance set (RegExp property-escapes:
~420-440 fixtures hang at the 30s batch timeout under 8-job load; the
TypedArray copyWithin handful) is build-bound (~6.7s fixture build, ~1µs
per element write + a per-char property-escape table scan per regex
test); a precomputed match table per property is the deferred fix.

### Cut 12 — per-call dispatch: env-constant sync skip, single record fetch, shared IC caches (measured 2026-08-22)

Fresh 3-run medians of `--bench` (release, interleaved with the A/B
builds below):

| Benchmark | Cut 11 | Cut 12 |
|---|---|---|
| arithmetic | 183ms | 178ms |
| property access | 241ms | 237ms |
| string concat | 58ms | 59ms |
| array iteration | 317ms | 312ms |
| function calls | 625ms | 431ms |

Function calls 625ms → 431ms (~1.45x) on the way to the 40x-vs-Node
`--jitless` gap (16ms); suite ~1.47s → ~1.21s.

The function-call path was decomposed per call (Cut 12 stage 1):

1. **Certified-body env constancy** — a certified body's `lexical_env`
   never changes (every binding is a frame slot; no `with`/`try`/
   `switch`/`for-in`/`for-of`/`using`), so the dispatch loop skips its
   per-step `running_context_mut` + `Rc::ptr_eq` sync for it
   (`CompiledBody::env_constant`). `fast_block` was extended from
   empty blocks to any block whose lexical declarations are all frame
   slots, so `let`/`const` blocks in certified bodies stop allocating
   envs.
2. **Single `ecma_functions` fetch** — `call_inner` fetches the record
   once and hands the certified fast path its fields (ir/environment/
   realm/strict) as `FastCallData`, dropping the second HashMap hit per
   EcmaScript call; `is_eval_function` skips the intrinsics lookup for
   non-function and EcmaScript callees (`%eval%` is a builtin);
   `with_agent` is a no-op when the agent is already current (nested
   certified `run_inner` entries), removing the redundant TLS swap.
3. **Agent-shared IC caches** — the global-var cells and the P3
   member/element cells moved off the per-call `Vm` onto the `Agent`.
   They were re-created (and ~900 bytes re-zeroed) by every `Vm::new`,
   so each function call and script evaluation started cold; they are
   re-validated against the current realm's global and each object's
   property vector on every access, so sharing them across Vms — and
   across realms — is exact. A function's member accesses now hit its
   caller's warmed cells: a 1M-call `f(o) { return o.a }` probe
   0.61s → 0.52s (~15%), and a global-member-through-call probe
   1.08s → 0.93s (~14%), at zero cost to pure-call shapes.

   The originally planned stage 2b — running a certified callee on the
   caller's `Vm` (a frame-stack split, saving/restoring the ~35
   body-specific fields around the recursive dispatch run) — was
   implemented and A/B'd: it warmed the member shapes (~10%) but cost
   ~6% on pure-call and recursion shapes (the save/restore of ~35
   fields outweighs the `Vm::new` init it replaces, now that the cell
   zeroing is gone). The agent-shared caches deliver the warmth at zero
   per-call cost, so the split was reverted. The certified context push
   also shares one `EnvRef` between `lexical_environment` and
   `variable_environment` (a certified body never reads the latter).

Conformance after Cut 12: zero regressions across the sweeps (language
23,690/0/34; built-ins 23,272/0/154 — the 386 hangs are the known
RegExp property-escape + TypedArray copyWithin set; annexB 1,086/0/0).

The remaining function-call cost (~330ns for an `empty()` call, 2M
calls) is the certified call machinery: the `ExecutionContext` push
(~4-6 Rc clones), the `Vm::new`/drop of the frame + env-stack Vec (a
16-byte alloc per call), `setup_frame`, and the `ecma_functions`
HashMap hit. The next lever is eliminating the per-call `Vm` (a
grow-down value stack with a frame boundary, keeping the certified
callee's control stacks isolated), or caching the certified record's
fields on the function object.

### Cut 13 — identity hashing, empty-frame skip, inline env stack (measured 2026-08-22)

Three per-call costs from the `empty()` decomposition (~330ns/call after
Cut 12):

1. **Identity hasher for `ecma_functions`** — the keys are already-
   unique u64 function ids, so std's SipHash (default) burned ~20ns per
   `Call` hashing them; a 15-line `IdentityHasher` (wrap in
   `BuildHasherDefault`) turns that into an identity fold. The HashMap
   still probes and compares keys, so collisions are handled exactly as
   before.
2. **Skip `setup_frame` for zero-slot frames** — a certified body with
   no bindings (`frame_size == 0`, e.g. `empty()`) never reads the
   frame; `Vm::new` already left the inline buffer in place, so the
   slot-by-slot setup (with its per-slot TDZ checks) is skipped.
3. **Inline `EnvStack`** — the per-call `Vm` allocated a one-element
   `Vec` for the scope-environment stack in `Vm::new` (a heap
   round-trip every call, and again on every `EnterBlock` push). The
   stack is now an 8-entry inline `[Option<EnvRef>; 8]` with a heap
   fallback for deep nesting, so the base env and shallow block envs
   cost no allocation at all. The two disposable-resource drains were
   rewritten against the new storage (the `CatchBind` drain is
   destructive, so it truncates after collecting).

Measured (release, `empty()` 2M calls — isolates the call path; the
machine was load-shifted ~5-8% for the bench runs): 330ns → ~280ns per
call; the `--bench` function-calls row ~420-440ms under load (was
~431ms baseline). Zero conformance regressions (language 23,690/0/34;
built-ins 23,268/0/154 — 390 hangs, the known RegExp property-escape +
TypedArray copyWithin set; annexB 1,086/0/0).

The remaining ~280ns/call is the `ExecutionContext` push (~4-6 Rc
clones), the `Vm::new`/drop of the remaining fields, and the certified
record's field clones. A grow-down value stack with the caller's Vm
(the frame-stack split) was A/B'd at Cut 12 and regressed; the next
candidates are caching the certified record's hot fields on the
function object (a direct-mapped id → (ir, env, realm, strict) cache
on the Agent) and trimming the context push.

### Cut 14 — global-cell fast path and V8-interpreter study (measured 2026-08-22)

The `--bench` rows are all top-level loops over declared global `var`s
(`n`, `i`, `s`, `o`, `f`), so the global-access path and the loop head
dominated every row. Fresh 3-run medians of `--bench` (release,
interleaved with probes):

| Benchmark | before | after |
|---|---|---|
| arithmetic | ~196ms | 70ms |
| property access | ~247ms | 116ms |
| string concat | ~60ms | 55ms |
| array iteration | ~320ms | 255ms |
| function calls | ~440ms | 280ms |

Per-iteration decomposition (1M iterations): the empty loop head
(`i < 1M` test + `i++` + jump) was ~180ns and is now ~48ns; the
`n += i*2` body adds ~25ns. The wins:

1. **Direct-mapped global cells** — `global_cells` was a `HashMap`
   (even identity-hashed, ~10ns probe); it is now a 32-entry
   `[Option<(AtomId, usize)>]` array indexed by `name & 31`, and the
   `load`/`store`/`update` fast paths probe it with a single compare.
   The per-access context-stack walk + realm-global clone is gone too:
   the `Vm` caches the running context's global object on first access
   (a body's realm cannot change while its own steps run).
2. **Fused global update** (`++`/`--`) — `IncGlobal`/`DecGlobal`/
   `UpdateGlobal` previously composed `load_global_value` +
   `store_global_value` (two probes, two `RefCell` borrows); they now
   read/update/write through one borrow, with a Number fast path that
   skips the `to_numeric` call.
3. **Number loop test** — `jump_if_rel_global`/`jump_if_rel_imm`
   compare a Number counter against the numeric limit directly off the
   cell/slot (Rust's NaN-false semantics match JS), falling back to
   `apply_binary` only for non-number counters.
4. **Env-constant scripts** — `compile_statements` set
   `env_constant: false` unconditionally, so every fast-script step
   paid the running-context sync (a `last_mut` + `Rc::ptr_eq`). The
   `Compiler` now tracks whether any emitted step switches the lexical
   environment (`EnterBlock`/loop/catch/with envs) and sets
   `env_constant` accordingly; the bench scripts are env-constant, so
   the per-step sync is skipped.
5. **Dispatch cleanup + inline arithmetic** — the loop's double
   bounds check became a single `get`, and `BinaryImm` inlines the
   number-number arithmetic path (two tag checks + a direct op) for
   Sub/Mul/Div/Rem, falling back to `apply_binary`.

Conformance: zero regressions (language 23,690/0/34; built-ins
23,259/0/154 — 399 hangs, the known RegExp property-escape +
TypedArray copyWithin set; annexB 1,086/0/0).

**The V8 `--jitless` study** (the vendored checkout in `v8/`): V8's
ignition interpreter runs the same loops at ~11-16ms/1M (~1ns per
bytecode) because (a) the dispatch is a computed-goto jump table over
minimal assembly handlers, (b) global loads are **PropertyCell**
indirections — the feedback vector holds a weak cell pointer, the cell
holds the value, so a read is two loads with no per-access validation
(a redefinition replaces the cell and marks the old one with a hole),
and (c) the interpreter is an accumulator machine — binary ops read
one operand from the accumulator and one from a register, with no
stack push/pop per step.

The path from the current ~10ns/step to V8's ~1ns/step is therefore:
1. **Cell-backed global bindings** — hold the script's declared `var`s
   behind a stable `PropertyCell`-like object (value lives in the
   cell; redefinition replaces it), so a global load/store is a cached
   cell pointer + one field load — no `RefCell` borrow, no key
   re-validation. This is the single biggest remaining lever for the
   global-path benches.
2. **Accumulator/register execution** — drop the per-step value-stack
   push/pop for the common unary/binary shapes: operand and result
   registers encoded in the step, like ignition's accumulator.
3. **Fused loop step** — recognize the canonical
   `for (var i = INIT; i <op> LIMIT; i++) BODY` shape and run the
   test + body + increment with one dispatch per iteration (the
   dispatch-loop extraction this requires is the mechanical part).

**Cell-backed globals were implemented and reverted (measured
regression).** A `GlobalCell { value, writable, valid }` was attached to
`Property`, every ordinary set/define kept it in sync (or invalidated it
on a redefine — V8's hole), and the fast path read/wrote the cell with
no property-vector borrow; two variants were A/B'd: a mirror write
keeping `Data.value` fresh (the fast write then paid TWO validation
passes) and a cell-authoritative variant with the slow reads routed
through the cell (`Property::value`, `to_descriptor`, `ordinary_get`,
`get_property`'s fast path, `member_cell_get`). Both regressed: the
empty loop head 48ns → 50-59ns and arithmetic 70ms → 78-96ms. The
reasons: the `RefCell` borrow this engine replaces is ~2ns on a hot
cache line (V8's heap has no borrow flags), the cell adds an `Rc`
indirection plus `valid`/`writable` checks per access, the 16-byte
`Property` growth widens every property vector, and the mirror variant
doubled the write path. The validated-slot fast path (probe + borrow +
key match) remains the best fit for this object model; the V8 cell
advantage is specific to its GC-managed, borrow-free heap.

### Cut 15/16 — fused loop head, script var slots, inline Value ops (measured 2026-08-22)

Fresh 3-run medians of `--bench` (release):

| Benchmark | Cut 14 | now |
|---|---|---|
| arithmetic | 70ms | 43ms |
| property access | 116ms | 83ms |
| string concat | 55ms | 43ms |
| array iteration | 255ms | 233ms |
| function calls | 280ms | 276ms |
| suite | ~780ms | ~680ms |

1. **Fused canonical loop head (`Step::FastLoopHead`)** — the
   `for (var i = INIT; i <op> LIMIT; i++/i--)` on one fast binding now
   runs increment + re-test + back-jump in a single dispatch (the body
   dispatches inline), replacing `IncGlobal` + `JumpIfLtGlobalImm` +
   `Jump`. Measured: **no gain on its own** — the savings (2 of 3
   dispatches) are inside noise; the per-iteration cost was the global
   load/store work, not the dispatch count. Kept (it removes steps and
   composes with the slot work below).
2. **Certified-script var slots (`ScriptSlots`)** — the closed-world
   script's declared `var`s live in frame slots for the whole run
   (borrow-free; the prologue loads each var's current global value,
   the epilogue writes back the assigned ones). The per-access cost
   drops from the global fast path (direct-mapped probe + `Rc` clone
   of the global handle + `RefCell` borrow + key compare) to a plain
   frame read/write — the V8 context-slot model for non-escaping
   script vars. Qualification is a closed-world scan: no
   function/class decls, no `call`/`new`, no `this`/closures, no
   `globalThis`-family identifiers, no destructuring patterns (a
   declared-but-never-assigned read-only global like `Infinity` gets
   no write-back). Scripts failing the scan keep the Cut 14 global
   path unchanged.
3. **`#[inline]` on the NaN-boxed `Value` hot surface** (crux) —
   `is_double`/`tag`/`is_uninitialized`/`as_number`/`is_number`/
   `Boolean`/`Number` plus `Clone::clone` and `Drop::drop`. With no
   LTO, every cross-crate call was a real call on the hottest path;
   this was the largest single win (~25-30% on the loop rows: empty
   loop 50 → 37ms, arithmetic 97 → 64ms before the slot change
   compounded).

Conformance: zero regressions — language 23,690/0/34, built-ins
23,255/0/154 (403 hangs: the known RegExp property-escape + TypedArray
set, unchanged in kind; the count is the performance signal at the 30s
ceiling, see the RegExp table item in Deferred milestones), annexB
1,086/0/0.

Remaining `--bench` levers, in measured order: (1) the accumulator
loop counter (the head is still ~15 ops/iteration round-tripping the
frame; V8 keeps the counter in a register), (2) array-iteration fast
path (the array row's 233ms is the per-element for-of `next()` call
machinery), (3) the per-call machinery (Vm::new + ExecutionContext
push + TLS re-entry — the Cut 12 frame-stack-split analysis), (4) the
RegExp property-escape match table (the built-ins hang set).

### Cut 17 — accumulator loop counter (measured 2026-08-22)

`Vm` gained an `acc` field; a canonical loop whose counter is a frame
slot and whose body only reads it (or assigns/updates it in statement
position — a body scan certifies this) runs with the counter in `acc`
for the loop's duration: `FastLoopBind` loads it once, the head
(`FastLoopHead { var: Acc }`) increments and re-tests it in place, the
body's redirected reads/writes use `PushAcc`/`PopAcc`/`IncAcc`/`DecAcc`,
and `FastLoopStore` writes it back at the exit (`break` lands on the
store). This is the V8 register-counter model for the loop head.

Measured: **~0 (within noise)** — arithmetic 43→43ms, empty loop
37→36ms. The frame-slot access the accumulator replaces was already
cheap; the per-iteration floor is the ~15-op head machinery (dispatch
+ `Value` inc/test round-trip: clone, `is_uninitialized`, `as_number`,
`Value::Number` construction) at ~2ns/op. Kept anyway: it removes the
frame round-trip, composes with the register-machine work, and is
conformance-clean.

The real head cut requires the counter to live as a raw `f64` (no
`Value` round-trip per iteration) with a specialized step — the
register machine below — or a cheaper dispatch loop.

Remaining `--bench` levers, in measured order: (1) the raw-f64 loop
counter (specialized steps, no `Value` inc/test machinery), (2)
array-iteration fast path (the array row's 233ms is the per-element
for-of `next()` call machinery), (3) the per-call machinery (Vm::new +
ExecutionContext push + TLS re-entry — the Cut 12 frame-stack-split
analysis), (4) the RegExp property-escape match table (the built-ins
hang set).

Three rows were added to `--bench` (2026-08-22) to cover the Cut 3
continuation certification shapes the original five predate: `closure
capture` (1M calls through a closure reading its enclosing body's
captured binding — the context-chain slices, ~320ms), `per-iteration`
(100k calls to closures created over a `for (let i ...)` head — the
per-iteration machinery, ~51ms), and `construct churn` (100k `new C`
on a constructor reading `this` — the this slots + construct fast
path, ~452ms). The 2026-08-18 gate baselines above cover only the
original five rows.

### Cut 18 — for-of/for-in heads certification (2026-08-22)

The last certification gap from `bytecode-plan.md` §8 item 7: a body
containing a `for-in`/`for-of` with an ident head now takes the fast
path instead of bailing to the env path. A `var` head binds the
slot/global/context directly per element/key; an uncaptured lexical
head re-inits its flat slot per iteration; a captured lexical head runs
the per-iteration env machinery (fresh copies the body's closures
observe, mirroring the certified `For`). The head's TDZ environment is
skipped — the slot/context marker reproduces the RHS `ReferenceError`.
Destructuring/expr/`using` heads and async for-of keep the env path.

Measured: the existing `--bench` rows don't move — they are scripts,
and the script path already certified `var`-head for-of (the dense
array fast path binds the head via `ForOfBindGlobal`/`ForOfBindLocal`).
The win is real-world function bodies: `for (const x of arr)` /
`for (let k in obj)` loops inside functions no longer kick the whole
body onto the env path. Conformance at baseline: language 23,690/0/34,
annexB 1,086/0/0, built-ins 23,216/0/154 (the known RegExp
property-escape + TypedArray hang cluster); workspace tests 4311/0
including a new fast-path step-stream test asserting the certified
steps.

### Cut 19 — per-call Vm reuse (measured 2026-08-22)

The per-call machinery's construction cost: every call to a compiled
body built a fresh `Vm` (~30 fields, ~20 empty `Vec`s, the 8-slot
inline frame, the inline env stack) and tore it down. The agent now
keeps a free-list of `Vm`s (`vm_pool`); the ordinary call, construct
fast path, and script/eval paths take one, run, and return it — the
reset clears the Vec stacks in place (capacity kept), re-points the
inline env stack, and leaves the frame stale (the next run's
`setup_frame` overwrites every slot below `frame_size`; a
`frame_size`-0 body never reads it). Suspended generator/async/module
states still own their Vm (never pooled), so no Vm aliases a live
suspension.

Measured (3-run medians, vs the immediately-prior tree):

| Row | before | after |
|---|---|---|
| function calls | ~300ms | **270ms** (-10%) |
| closure capture | ~312ms | **264ms** (-15%) |
| construct churn | ~549ms | **454ms** (-17%) |
| per-iteration | ~52ms | 49ms |

Arithmetic/property/concat/array flat (they don't call). The calls row
is still ~11x node `--jitless` — the per-call `ExecutionContext`
push/pop, the record fetch, and the dispatch-loop entry remain; the
structural fix is the Cut 12 frame-stack-split (one dispatch loop over
a frame stack, no per-call Vm/context). Conformance at baseline:
language 23,690/0/34, annexB 1,086/0/0, built-ins 23,202/0/154 (known
hang cluster); workspace tests 4311/0.

### Cut 20 — certified-construct fast path (measured 2026-08-23)

`ordinary_construct` now mirrors the certified call: a base constructor
with a certified body (empty `context_names`), no instance fields, and
no private methods skips the whole slow path — no `FunctionEnv`, no
`function_declaration_instantiation`, no `initialize_instance_elements`,
and the record is borrowed, not deep-cloned (the params/body AST clones
the slow path pays per construct disappear). `this` comes from
`construct_this_object` (OrdinaryCreateFromConstructor, extracted from
the slow path), the slim `ExecutionContext` matches the certified
call's (`script_or_module`/`source`/`private_environment` are never
consulted by a certified body), and the base-constructor return rule
(object/function return wins, else `this`) is applied directly to the
body completion. Class constructors with instance fields/private
methods and derived constructors keep the slow path — the
`fields`/`private_methods` guards stand in for the skipped
instance-element machinery.

Measured (5-run medians, vs the Cut 19 tree):

| Row | before | after |
|---|---|---|
| construct churn | 454ms | **198ms** (-56%) |

Calls/closure/per-iteration/arithmetic flat (they don't construct). The
construct row is now ~36x node `--jitless` (was ~82x) — the remaining
cost is the `this`-object allocation, the slim context push/pop, and
the dispatch-loop entry, all addressed by the Cut 12 frame-stack-split.
Conformance at baseline: language 23,690/0/34, annexB 1,086/0/0,
built-ins 23,201/0/154 (known hang cluster); workspace tests 4311/0.

### Cut 21 — certified-call context trim and fused for-of slot binds (2026-08-23)

Two per-call/per-element trims on the certified path, both strictly
less work with no semantic change (gains sit at the machine's bench
noise floor — ~10-20ns per call/element):

1. **Certified-call context trim** — the `ExecutionContext` pushed per
   certified call cloned the running function (`function` field). The
   only certified-path reader of that field is a sloppy body's mapped
   `arguments` creation (`Step::CreateArguments`, for `callee`); every
   other reader is excluded by certification (SuperCall is
derived-only). The clone is now made only when the body uses
`arguments` in sloppy mode — the bench call/closure/construct shapes
push `None`.
2. **Fused for-of step + slot bind** — a certified for-of whose ident
   head resolves to a frame slot emits `ForOfNextBindLocal` instead of
   `ForOfNext` + `ForOfBindLocal`: the element writes the slot
directly, one less dispatch and no value-stack round-trip per element
(the generic iterator path lands the slot the same way). The `--bench`
array row is a script, whose for-of is a separate pre-existing path,
so the row does not move; the win is function bodies (`for (const v of
arr)` in a certified body).

Measured (5-run medians, release): all `--bench` rows within noise
(function calls ~255ms, closure capture ~256ms, construct churn ~192ms
— the trims are ~1-2% each here; the empty-call probe is flat).
Conformance at baseline: language 23,690/0/34, annexB 1,086/0/0,
built-ins 23,216/0/154 (442 hangs, the known load-tied cluster);
workspace tests 4311/0.

### Cut 22 — write-side chain cache: fresh-property stores define directly (measured 2026-08-23)

The construct decomposition showed `this.x = x` at ~470-640ns: a store
on a fresh object runs the full `[[Set]]` — own-scan miss, prototype
chain walk (re-entrant scans + Rc clones per link), then the
descriptor/validate machinery before appending. The walk only matters
when the chain holds an accessor or a non-writable data property for
the key — a writable-data link stops `[[Set]]` at the same "define on
the receiver" outcome, absent links just continue.

A small direct-mapped Agent cache (`member_store_cells`) now records
"the chain from this prototype holds no accessor/non-writable for this
key", re-validated exactly against the chain links' generations: every
own-property mutation (define/delete) and prototype change bumps a
per-object `generation` counter, so a hit re-walks the chain reading
only the links' generations (2-3 clones + compares) instead of the full
property scans. On a verified hit — and only for a plain Ordinary
receiver whose own scan misses — the store appends the writable data
property directly (`fresh_data_define`), skipping the descriptor/
validate machinery too. Any doubt (an exotic receiver or chain link, an
accessor/non-writable anywhere, a non-extensible receiver, an existing
own property) falls back to the full `[[Set]]`.

Measured (5-run medians, release, vs the Cut 20/21 tree):

| Row | before | after |
|---|---|---|
| construct churn | ~192ms | **~152ms** (-21%) |
| `{}`+store 1M (probe) | ~730ms | **~355ms** |

The fresh-object store drops ~470ns → ~95ns; `this.x = x` in the
constructor drops to ~170ns. Calls/closure/array flat. Conformance at
baseline: language 23,690/0/34, annexB 1,086/0/0, built-ins
23,216/0/154 (442 hangs, the known load-tied cluster); workspace tests
4311/0. The remaining construct cost is the `this`-object allocation
(~260ns), the read of `o.x` on a fresh object (~200ns — the GET cache
is object-id-keyed), and the call machinery — the shapes work the
write-side cache is a slice of.

### Cut 23 — proto-keyed read-cell fallback (measured 2026-08-23)

The read of a field on a fresh object (a constructor's new `this`) was
~200ns: the `member_cells` GET cache is object-id-keyed, so every fresh
object missed and fell to the full Get. Fresh instances of the same
constructor share their prototype's shape — `x` sits at the same slot
in every `new C()` — so `resolve_member_cell` now also records the slot
under `(prototype id, name)` (`member_proto_cells`), and
`member_cell_get` falls back to that entry on an object-id miss,
validated per access against the instance's own property vector (a
divergent layout misses and re-resolves, exactly like `member_cells`).

Measured (medians, release, vs Cut 22):

| Row | before | after |
|---|---|---|
| construct churn | ~152ms | **~135ms** |
| `new C(i)`+read probe | ~104ms | **~94ms** |
| `{x:i}`+read 1M probe | ~633ms | **~503ms** |

The fresh-object field read drops ~185ns → ~55ns. Property/array/calls
flat. Conformance at baseline: language 23,690/0/34, annexB 1,086/0/0,
built-ins 23,213/0/154 (445 hangs, known load-tied cluster); workspace
tests 4311/0.

### Cut 24 — for-of fast-verdict cache: skip the iterator-method chain walk (measured 2026-08-23)

An array-iteration probe showed `for_of_begin` at ~1.5µs per call —
160ms of the 237ms array row. Every for-of entry ran `get_method`
(a full `@@iterator` read: two prototype-chain walks + accessor scan)
plus three intrinsics lookups and the iterator-infra checks, even for
a plain Array that had been iterated a million times already.

`for_of_begin` now takes the fast path without `get_method`: a plain
Array with no own `@@iterator` whose prototype is the realm's
%Array.prototype%, plus a gen-validated cached "the Array-iteration
infrastructure is stock" verdict — %Array.prototype%.@@iterator is the
intrinsic, %ArrayIteratorPrototype% has the stock `next`, and no
`return` on the AIP chain. The verdict stores the three shared
objects' generation counters (Cut 22's mechanism): a probe re-reads
them (~3 Rc clones + compares); any mutation — `a[Symbol.iterator]`
patched, `Array.prototype[Symbol.iterator]` replaced, a `return` added
to the chain — bumps one and re-resolves the full check. Custom-proto
arrays and every other doubt fall to the unchanged generic path.

Measured (5-run medians, release, vs Cut 23):

| Row | before | after |
|---|---|---|
| array iteration | ~237ms | **~121ms** (-48%) |
| for-of begin+done 100k (probe) | ~160ms | **~49ms** |

The begin is now ~490ns/outer-iteration (from ~1.5µs), the rest the
step machinery. Construct/property/calls flat. Conformance at
baseline: language 23,690/0/34, annexB 1,086/0/0, built-ins
23,216/0/154 (442 hangs, known load-tied cluster); workspace tests
4311/0. The suite is now ~1.01s (from ~1.15s before this cut).

### Cut 25 — certified leaf calls inline on the caller's Vm (measured 2026-08-23)

A call-family decomposition (Cut 20 follow-up) put ~85-115ns of pure
machinery on every certified call: the execution-context push (~40ns),
`take_vm`/`return_vm` pool round-trip with the 25-field reset (~30-50ns),
the redundant nested `with_agent` TLS pair, the `running_context` env
clone, and the record lookup's triple `Rc` clone. A `f(x) { return x + 1 }`
call paid all of it. Cut 12's naive fix (save/restore all 35 VM fields per
call) lost ~6% — the full-frame copy dominated.

The re-attempt inlines only *certified leaves*: a certified body whose
compiled steps contain no re-entry (no `Call`/`CallFast`/`Construct`/
`SuperCall`/`TaggedTemplate`), no running-context read (no `LoadIdent`,
reference machinery, `this`-value, `new.target`, super, closure creation
— the leaf's closures would capture the CALLER's env), no environment or
iterator machinery, and no sloppy mapped `arguments` (its object reads the
context's `function`). Such a body cannot recurse, so the inline run is a
flat save/restore of the handful of fields a leaf can touch — `ip`, the
frame (swapped, not copied), the value-stack length, `completion`/
`completion_is_empty`, `acc`, `strict`, `body_context`, `chain_short`, the
`list`/`completion`/`var_ref`/`array_index` stack lengths, and `call_args`
(when the leaf observes `arguments`) — plus the pre-existing clean-site
guard: the caller's `try`/`pending`/for-of/for-in/destructure stacks and
env stack must be empty, so the leaf's own `return`/`throw`/`break`/
`continue` resolve against nothing but its own steps (they run through
`run_inner_inner` directly, so a leaf error propagates raw to the caller's
`run_inner`, which applies the caller's handler coverage, iterator close,
and disposal with the caller's `ip` restored — exactly a nested call's
path). A leaf runs with the caller's realm current, so the path is gated
on a single realm. The construct shape (`new C(x)`) inlines the same way:
`construct_this_object` + the base-constructor return rule, gated on the
certified-construct conditions (base kind, no fields/private methods) plus
`is_method`.

Measured (5-run medians, release, vs Cut 24):

| Row | before | after |
|---|---|---|
| function calls | ~256ms | **~174ms** (-32%) |
| closure capture | ~255ms | **~172ms** (-33%) |
| construct churn | ~135ms | **~118ms** (-13%) |

Calls and closure dropped ~82ms each (the per-call machinery is now
~90ns/call instead of ~197ns); construct drops the same pool/context
round-trip. Arithmetic/property/array/per-iteration flat. Conformance at
baseline: language 23,690/0/34, annexB 1,086/0/0, built-ins 23,656/0/154
(2 load-tied hangs — the `Script_-_Balinese`/`Myanmar` property-escape
generates pass individually); workspace tests 4311/0. The suite is now
~0.83s (from ~1.01s). One construct-path gap found and fixed by the
sweep: an object-literal method (`{ method() {} }`) is a certified leaf
whose `new` must throw "not a constructor" — the inline construct path
now checks `is_method`.

### Cut 26 — construct-this prototype cache and function-object member cells (measured 2026-08-23)

A construct-path decomposition (the `new C(i)` bench row) showed the
per-construct cost split three ways: OrdinaryCreateFromConstructor's
`prototype` read ran the full property path (~280ns — own-scan + chain
walk), the fresh object's creation paid the `%Object.prototype%`
intrinsics HashMap lookup per create, and — a surprise — every own-data
read on a FUNCTION value took ~310ns vs ~60ns on a plain object: the
P3 member cells only accepted `ValueKind::Object`, so a function's
`length`/`name`/`prototype`/custom properties fell through to the full
Get on every access.

- **Function member cells**: `member_cell_get`/`resolve_member_cell` now
  serve `ValueKind::Function` values through the function's underlying
  ordinary object (same slot→key→kind re-validation, same proto-keyed
  fallback) — `C.prototype`-style reads cache like any own data read.
- **Construct-this prototype cache**: `construct_this_object` caches the
  constructor's `prototype` read per function id on the agent, re-validated
  against the function object's generation counter (Cut 22's mechanism — a
  redefine/delete bumps it, so a stale entry re-reads). Proxies and other
  exotic newTargets stay on the uncached path (their `prototype` read can
  run traps).
- **`%Object.prototype%` intrinsics cache**: `Intrinsics::object_prototype`
  resolves the realm's `%Object.prototype%` once (the intrinsics table is
  fixed at bootstrap) and serves `ObjectBegin` and the construct fallback
  from a cached handle.

Measured (5-run medians, release, vs Cut 25):

| Row | before | after |
|---|---|---|
| construct churn | ~118ms | **~82ms** (-30%) |
| function calls | ~174ms | ~162ms |
| closure capture | ~172ms | ~165ms |

Function-property probe reads dropped ~310→~90ns; the object-literal
probe ~320→~140ns. Calls/closure moved a few ms (noise band);
arith/property/array/per-iteration flat. Conformance at baseline: language
23,690/0/34, annexB 1,086/0/0, built-ins 23,651/0/154 (7 load-tied
`RegExp/property-escapes/generated/*` hangs, the known slow set — the
15s sweep-timeout rule classifies them as real hangs); workspace tests
4311/0. The suite is now ~0.77s (from ~0.83s).

### Cut 27 — per-array for-of fast verdict (measured 2026-08-23)

An array-row decomposition (the `for (var v of a)` bench row) split the
cost between the for-of BEGIN (100k outer iterations × ~490ns ≈ 49ms —
the Cut 24 fast path still ran the own-`@@iterator` property scan, the
`%Array.prototype%` intrinsics lookup, and the prototype walk per begin)
and the per-element step machinery (~60ms for 1M `ForOfNext`s). The begin
dominated: the same array was re-verified 100k times.

`for_of_begin` now probes a per-array fast-verdict cell — (array id, array
generation, prototype id) — before the Cut 24 checks: a hit skips every
check except the cheap gen-validated stock-iterator probe. The array
generation (Cut 22's mechanism) catches an own `@@iterator` addition and
proto changes; the prototype's own mutations bump ITS generation, which
`for_of_fast_probe` re-validates per access. The cell is populated when
the full check passes; a miss re-runs the checks and re-resolves.

Measured (5-run medians, release, vs Cut 26):

| Row | before | after |
|---|---|---|
| array iteration | ~118ms | **~76ms** (-36%) |
| for-of begin 100k (probe) | ~49ms | **~3ms** |

The begin is now ~30ns/outer-iteration (from ~490ns). All other rows
flat. Conformance: language 23,690/0/34, annexB 1,086/0/0 at baseline;
built-ins 23,211/0/154 with 447 hangs — under the 15s sweep-timeout rule
every one is the >15s slow class (435 RegExp property-escapes generates,
5 CharacterClassEscapes, 4 Temporal argument-string-limits, 3 TypedArray
detached-coercion), all previously passing under the old 120s deadline
(sampled individually: they complete, just over 15s); zero new failures
it. Workspace tests 4311/0. The suite is now ~0.72s (from
~0.77s).

### Cut 28 — static reads for captured per-iteration heads (measured 2026-08-23)

The per-iteration bench row (`fns[j & 15]()` over arrows created in a
certified `for (let i...)` loop) paid a full env-chain walk per call: the
arrow's `i` reference compiled to `LoadIdent` (the per-iteration heads are
deliberately stripped from the closure's outer-chain entries — the capture
context's head slot is stale between iterations), so every call resolved
`i` through the runtime environment. The arrows were also NOT leaf-
eligible, so they paid the full certified-call machinery on top.

A closure created inside a certified per-iteration loop captures the
per-iteration env directly, and that env is its `lexical_env` at run time
— so the read can be static. The closure's metadata now carries a
`per_iteration_chain` (the `(head names, env hop offset)` of the loops open
at its creation site, threaded through `EcmaFunction` →
`CreateArrow`/`CreateFunction` → `compile_body`); `binding` resolves those
heads to the existing `LoadPerIteration`/`StorePerIteration`/
`UpdatePerIteration` steps (depth = the closure's own capture-context hop +
the chain entry's offset), and the per-iteration steps became leaf-eligible
(the inline run sets `lexical_env` to the leaf's env only when the leaf has
such steps — `CompiledBody::leaf_needs_env`). Nested loops, closures-in-
closures, and multi-head loops are covered by the depth bookkeeping; every
uncertain case still falls back to the env walk.

Measured (5-run medians, release, vs Cut 27):

| Row | before | after |
|---|---|---|
| per-iteration | ~47ms | **~23ms** (-51%) |
| function calls | ~162ms | ~172ms |
| closure capture | ~168ms | ~174ms |

One regression found and fixed during the cut: the Cut 27
`for_of_array_cells` (16 × 24-byte inline entries) bloated the Agent
struct's hot-field cache footprint and slowed the leaf-call path ~10ns/call
(isolated by A/B: shrinking the field restored the identity-leaf probe
166→165ms); the cache is now `Box`ed so the Agent holds an 8-byte pointer
instead. The call-family total (calls + closure + per-iteration) still
improved ~5ms net. Conformance at baseline: language 23,690/0/34, annexB
1,086/0/0, built-ins 23,211/0/154 (447 >15s hangs, the known slow class);
workspace tests 4311/0. The suite is now ~0.69s (from ~0.72s).

### Cut 29 — leaf-inline env clone only on the per-iteration path (measured 2026-08-23)

Cut 28 made the inline leaf run swap `lexical_env` to the leaf's env when
the leaf contains per-iteration steps. To keep the moved `body_env` alive
for that swap it changed `self.body_context.replace(body_env)` to
`body_env.clone()` — a clone executed on EVERY leaf call, even the common
leaf with no per-iteration reads. `EnvRef` is an `Rc<EnvRecord>`, so the
unconditional clone was two refcount atomics per call (fetch-add on clone,
fetch-sub on drop) — ~20-25ms on each call-family row.

The save sequence now clones only on the `leaf_needs_env` path: the false
branch restores the pre-Cut-28 move into `body_context`, the true branch
clones once for the swap (the branch itself is fully predictable and costs
nothing measurable).

Measured (release, vs the pre-fix tree):

| Row | before | after |
|---|---|---|
| function calls | ~190ms | **~166ms** |
| closure capture | ~186ms | **~168ms** |
| per-iteration | ~27ms | ~23ms |
| construct churn | ~84ms | ~80ms |

The call rows are back at the Cut 26 floor while per-iteration keeps its
Cut 28 win — the call family (calls + closure + per-iteration) is ~357ms vs
~403ms pre-fix. Conformance at baseline: language 23,690/0/34, annexB
1,086/0/0, built-ins 23,211/0/154 (447 >15s hangs, the known slow class);
workspace tests 4311/0. The suite is now ~0.68s.

### Cut 30 — skip the environment entirely for env-free leaves (measured 2026-08-23)

The inline leaf run still built a body context and swapped
`body_context`/`lexical_env` for every leaf, even one whose steps never
read an environment. `steps_are_leaf` guarantees a leaf can only touch
an env through context-slot steps (`LoadContextSlot` etc. resolve
`body_context`) or per-iteration steps (resolve `lexical_env`) — every
other env-reading step (identifiers, closures, env machinery, super,
`this`) is excluded. A new `CompiledBody::leaf_uses_env` flag records
whether the steps contain either family; when false, `run_leaf_body`
skips the body-context creation and both swaps entirely (with a
`debug_assert` on the invariant that an env-free leaf gets no env).

The call sites also cloned the callee's `[[Environment]]` per call
unconditionally (the borrow of the `ecma_functions` map can't coexist
with `&mut agent` in `run_inner_inner`, so the owned handle is
required) — now cloned only for a `leaf_uses_env` leaf, and passed BY
VALUE so the no-capture body moves it straight into `body_context`
(the Cut 29 clone was one of two Rc clones on the closure path).

Measured (release, vs the Cut 29 tree; the machine was load-noisy, so
medians over 6-7 runs):

| Row | before | after |
|---|---|---|
| function calls | ~172ms | **~165ms** |
| closure capture | ~183ms | ~172ms |
| per-iteration | ~23ms | ~22ms |
| construct churn | ~81ms | ~78ms |

Calls dropped ~8-9ms from the env skip plus ~4ms from the lazy call-site
clone; closure lost the second of its two per-call clones (the residual
delta is mostly load noise). The call family is now ~360ms and the suite
~0.67s. Conformance at baseline: language 23,690/0/34, annexB 1,086/0/0,
built-ins 23,211/0/154 (447 >15s hangs, the known slow class); workspace
tests 4311/0.

### Cut 31 — raise the rope flatten cap (measured 2026-08-23)

The string-concat row (`s += 'x'` × 100k, building a 100k-unit rope)
flattened the whole accumulated left side every 64 appends
(`ROPE_MAX_DEPTH`), copying ~156 MB of units over the run. The cap
only bounds drop/flatten recursion, so raising it 64 → 1024 cuts the
flatten copies ~16× (to ~10 MB) while the ≤1024-frame recursion stays
well inside the default 8 MB stack. (Right-append chains like `'x' + s`
were already unbounded — the cap only ever protected left-append
chains — so the hazard surface is unchanged.)

Measured (release, vs Cut 30):

| Row | before | after |
|---|---|---|
| string concat | ~46ms | **~20ms** (-57%) |

All other rows flat; the suite is ~0.64-0.66s. Semantics are unchanged
(the flatten is an internal representation detail — strings are
immutable), but the shared rope machinery warranted the full sweep.
Conformance at baseline: language 23,690/0/34, annexB 1,086/0/0,
built-ins 23,211/0/154 (447 >15s hangs, the known slow class); workspace
tests 4311/0.

### Cut 33 — cache the leaf/construct inline verdicts on the function record (measured 2026-08-23)

The leaf-inline eligibility checks in `do_call_fast` and `Step::Construct`
re-walked the callee record per call: the `ir` Option, `ir.leaf`, and the
`is_class_constructor`/`class_field_initializer`/`this_mode`/`is_method`/
`constructor_kind`/`fields`/`private_methods` flags. All of those are
immutable once the ir compiles, so the record now caches the two verdicts
(`leaf_inline`, `construct_inline`) at ir-compile time and the hot paths
read one bool.

One trap: a class constructor's `fields`/`private_methods` are populated
by `build_class` AFTER registration, so the cached construct verdict
computed at compile time was stale (a default class constructor with an
empty leaf body looked inlineable before its fields arrived) — the
verdict is recomputed when `build_class` sets them. The runtime class
tests caught it.

Measured (release, vs Cut 31; the machine was load-noisy, medians over
6 runs):

| Row | before | after |
|---|---|---|
| function calls | ~170ms | ~175ms |
| closure capture | ~169ms | ~173ms |
| per-iteration | ~22ms | ~22ms |
| construct churn | ~80ms | ~78ms |

The delta is inside the noise floor (the change is strictly-less-work:
~4-6 fewer record checks per call), consistent with the Cut 21
noise-floor-win precedent. Conformance at baseline: language 23,690/0/34,
annexB 1,086/0/0, built-ins 23,211/0/154 (447 >15s hangs, the known slow
class); workspace tests 4312/0.

### Cut 34 — leaf frames on the value stack, a leaf-record cache, and fused statement stores (measured 2026-08-23)

Three changes to the leaf-inline call path and the loop-body statements:

- **The leaf's frame is now a flat segment on the value stack**
  (`Vm::leaf_frame_base`) instead of a swapped `[Value; 8]` inline frame:
  the caller's `frame` is never copied out-and-back, and only the live
  slots (frame_size, not 8) are pushed — the ~256-byte swap and the
  128-byte zero-fill are gone. The frame accessors route through
  `frame_get`/`frame_get_mut`, which branch on the base (fully predictable
  on both paths).
- **A Boxed direct-mapped leaf-record cache** (`Agent::leaf_cache`,
  16 entries keyed by function id — ids are never reused, so no generation
  check): `do_call_fast` reads the compiled ir, strictness, and closure env
  from the cache instead of the `ecma_functions` HashMap on every call.
  Boxed per the Cut 27 lesson (an inline copy bloat the Agent's hot-field
  footprint).
- **`FusedStoreLocal`/`FusedStoreGlobal`**: a statement-position assignment
  to a fast binding stores AND sets the statement completion in one step,
  killing the `Dup` + `StoreLocal` + `SetCompletion` trio (2 fewer steps
  per assignment statement in a loop). The `statement_expr`/`expr_depth`
  compiler fields scope the fusion to the statement's own assignment (a
  nested assignment in an operand still leaves its value).

Measured (release, vs Cut 33; the machine was load-noisy, so the spread
is wide — the calm-period medians):

| Row | before | after |
|---|---|---|
| function calls | ~165-190ms | **~145ms** |
| closure capture | ~167ms | **~152ms** |
| per-iteration | ~22ms | ~21ms |
| construct churn | ~78ms | ~78ms |

Calls and closure both drop ~15-40ms (the frame segment is the bulk; the
cache and fused store are noise-floor). The remaining ~50ns/call is the
leaf dispatch + eligibility floor of the interpreter design — getting
calls/closure under 100ms would need a JIT or a leaner dispatch. The
built-ins sweep showed 10 more >15s hangs (457 vs 447), all in the known
slow RegExp-property-escape/decodeURI classes and all passing when sampled
individually — load-dependent classification wobble, not regressions.
Conformance at baseline otherwise: language 23,690/0/34, annexB 1,086/0/0;
workspace tests 4312/0.

### Cut 35 slice 1 — register-encoded leaf bodies (measured 2026-08-23)

The first slice of the register-bytecode plan (Cut 3): the hot leaf bodies
(`return x + 1`, `(y) => x + y`, `() => i`) lower to a dedicated register
op set (`LeafOp`) and run on a small executor (`run_leaf_regs`/
`run_leaf_ops`) instead of the step dispatch loop:

- **A `LeafOp` set over a single accumulator** (`Vm.acc`) plus the leaf's
  frame segment, capture context, and (for per-iteration reads) lexical
  env: `LoadReg`/`LoadContext`/`LoadPerIter`/`LoadConst`, the binary ops
  (`BinReg`/`BinContext`/`BinPerIter`/`BinImm`/`BinConst`, with the
  `BinaryImm` number-number inline), `StoreReg`, and `ReturnAcc`. The
  lowering (`lower_leaf_ops`) accepts only the left-leaning straight-line
  shapes the hot bodies produce; anything else keeps the step path
  (conservative — a register body is a strict subset).
- **A minimal save/restore**: a register body touches only `acc`, the
  frame, and the env fields — never `ip`, completion, `strict`,
  `chain_short`, the array-index stack, or the arguments slice — so the
  per-call swap shrinks to two fields plus (for captured reads) the two
  env slots.
- **The call path flattens**: `do_call_fast` runs a register leaf directly
  on `run_leaf_regs` (no `run_leaf_call`/`run_leaf_body` indirection, no
  completion round-trip — a register body always completes `Return`); the
  `OrdinaryCallBindThis` logic moved to a shared `bind_this_value` helper.
- **Frame aliasing** (when every frame slot is a present parameter — no
  `this` slot, no var/TDZ slots, all args supplied): the frame overlays
  the caller's argument region on the value stack (`LeafFrame::Alias`), so
  there is no argument copy and no frame push; the caller's truncate
  discards the aliased slots. Missing-arg/this-slot bodies push the frame
  from a copied buffer (`LeafFrame::Pushed`) as before.
- **Context reads mirror `context_chain_env`**: `LoadContext`/`BinContext`
  skip context-transparent envs (a named function expression's
  self-binding scope, a per-iteration copy) before reading the capture
  context — the step path's depth-0 walk, reproduced exactly.

Measured (release, vs Cut 34; calm machine, 5-run medians):

| Row | before | after |
|---|---|---|
| function calls | ~148ms | **~101ms** |
| closure capture | ~157ms | **~105ms** |
| per-iteration | ~20.6ms | ~16.6ms |
| construct churn | ~78ms | ~78ms |

Calls drop 148→~101ms (-32%) and closure 157→~105ms (-33%), landing on
(or just above) the sub-100ms target; the isolated leaf call cost is
~50ns (was ~100ns for the step path, ~74ns after the flatten). Construct
churn is unchanged — `this.x = x` bodies use member machinery, not yet
register-encoded (a later slice). Per-iteration drops too (the
`() => i` body is now two register ops). The 67-case behavior probe
(`scratch/leaf_regs_probe.js`) covers the register path, the aliased and
pushed frames, captured/per-iteration reads, throwing binaries, and
caller-with-try/for-of state — all pass. Conformance: language
23,724/0/34, annexB 1,086/0/0, built-ins 23,812/0/154 with 440 >15s
hangs in the known slow RegExp-property-escape / Temporal-argument-
limits / detached-typed-array classes, all sampled individually PASS
(load-dependent batch classification); workspace tests 4312/0.

### Cut 35 slice 2 — fused global calls + dead-guard skip (measured 2026-08-23)

The second slice attacks the call site itself, two dispatches at a time:

- **`CallFastGlobal`** — a plain call to a declared top-level `var`/function
  global with an `undefined` receiver (`f(x)`, no `?.`, no `with`, no
  `eval`, not inside an optional chain, ≤ 2 plain args) fuses the receiver
  push and the callee load into the call step: the handler reads the
  global cell and passes `undefined` as `this` instead of the stack
  round-trip (`Push(undefined)` + `LoadGlobal` + `CallFast` → one step).
  The read goes through the existing `load_global_value` cache, and the
  leaf-inline path still runs (the frame aliases the argument region the
  same way). The compile-time guard is a certified-script-only shape —
  function bodies (`compile_body`) carry no `script_globals`, so the fuse
  never fires inside them.
- **The `chain_depth == 0` guard skip** — `compile_call_args_guarded` no
  longer emits the `JumpIfChainShort`/`Jump` pair when no optional chain
  is open: `chain_short` is set only inside a chain and cleared by the
  outermost chain node's `ClearChainShort`, so a guard at `chain_depth ==
  0` is provably dead. This also removes the guard from paren'd chain
  callees (`(a?.b)()`, `(a?.(x))(y)`), where the chain ends at the paren
  and the call must run on the chain's value (throwing on `undefined`)
  rather than being skipped.

Measured (release, 6-run medians; the earlier rows' ~98/103ms baselines
were the same guard-skip + register-leaf build):

| Row | before | after |
|---|---|---|
| function calls | ~98ms | **~86ms** |
| closure capture | ~103ms | **~90ms** |
| per-iteration | ~15.8ms | ~15.9ms |
| construct churn | ~77ms | ~74ms |

Both global-call rows drop ~12-13% (every run below the old baseline;
per-iteration/construct call member targets or `new`, so they are
unaffected). Isolated call cost is unchanged (the leaf body already ran at
~50ns); the saving is the two dispatches per call in the loop.

**Known tradeoff**: spec 13.4.3 step 2 (`GetValue` of the callee) runs
before step 4 (the arguments), but the fused handler reads the global cell
only after the args are on the stack — so a declared global redefined as
an accessor with side effects would observe the reversed order. This is
unobservable for the certified-script model the fuse requires (declared
top-level vars are data properties; the direct-mapped `global_cells` cache
already trusts them), and no fixture exercises it — the full sweep stays
clean.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (load-
dependent count); the 20-case fused-call probe
(`scratch/callfast_global_probe.js`) covers zero/one/two-arg leaves,
sloppy/strict `this`, non-leaf and recursive globals, reassigned callees,
and the non-fused fallbacks (3 args, spread, member, local).

### Cut 35 slice 3 — certified functions on the frame-slot path (measured 2026-08-23)

The Cut 16 frame-slot path (declared vars live in frame slots, the loop
counter in the accumulator) rejected any script containing a function
declaration or call — a callable could observe the stale global object
while the slots are authoritative. Slice 3 extends it to scripts whose
callables are provably **global-blind** (`analyze_script_scope` +
`certified_functions` in `ir.rs`):

- **Certified functions** (fixpoint): a top-level function declaration
  certifies when its body never references a declared var (except another
  certified function's stable entry-instantiated global binding), never
  calls an uncertified function, and contains no `this`/`super`/closures
  that could observe the global object, `eval`, `with`, `try`, `switch`,
  `for-in`, `using`, or `globalThis`-family identifiers. A body may read
  params/locals and undeclared names (real, never-stale global
  properties). Recursion never certifies; an assigned function name is
  never a candidate.
- **Certified values**: a certified closure (global-blind function/arrow
  expression), a literal, or a certified-call result (the callee's return
  value is itself certified — fixpoint over the declarations' `return`s).
  A var assigned a certified value (`var f = make(2)`) may be called at
  the top level; multiple assignments AND, compound/`++`/`--` marks the
  var unknown.
- **Certified functions stay global bindings** (never slotted): their
  stable entry-instantiated function objects live on the global object
  (`global_declaration_instantiation` hoists them), so a certified body
  reading another certified function's name is safe, and top-level calls
  to them still fuse (`CallFastGlobal`). The `FunctionDecl` statement
  compiles to nothing on the frame path (the entry instantiation did the
  work).

Measured (release, 6-run medians vs the slice-2 build):

| Row | slice 2 | slice 3 |
|---|---|---|
| function calls | ~86ms | **~65ms** |
| closure capture | ~90ms | **~80ms** |
| arithmetic | ~38ms | ~35ms |
| per-iteration | ~15.9ms | ~15.3ms |
| construct churn | ~74ms | ~74ms |

Both call rows drop further — the calls loop becomes `FastLoopBind`/
`FastLoopHead{Acc}` + `LoadLocal` + `CallFastGlobal` + `FusedStoreLocal`
(one global-cell callee read per iteration), the closure loop runs
entirely on frame/acc (`LoadLocal f` + `CallFast`, zero global access).

**The gate is all-or-nothing per script**: any uncertified function or
call keeps the whole script on the global path (the frame-slot staleness
window is unobservable only when every callable is global-blind). The
15-case probe (`scratch/certified_fns_probe.js`) covers the stale-read
regression (a function reading a declared var must see the live value,
not the stale slot), `globalThis`/closure/`this` bodies, certified-
function-to-certified-function calls, recursion, reassigned names,
certified-value vars (single and re-assigned), and uncertified-value
assignments — all pass.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 4 — fused slot-callee calls (measured 2026-08-23)

`CallFastSlot` extends the fused-call shape to a callee held in a frame
slot (a certified-value var like `var f = make(2)`): the receiver push and
the slot load fuse into the call step, so the closure loop runs entirely
on frame/acc — `FastLoopBind`/`FastLoopHead{Acc}` + `LoadLocal` +
`CallFastSlot` + `FusedStoreLocal`, zero global-object access per
iteration. The closure row converges with the calls row:

| Row | slice 3 | slice 4 |
|---|---|---|
| function calls | ~65ms | ~65ms |
| closure capture | ~80ms | **~67ms** |

**Ordering fix for the global fuse**: the probe for the slot fuse exposed
that `CallFastGlobal` (slice 2) had the same spec-13.4.3 ordering hazard
on the *global-only* path — the fuse fires for any declared global callee
there, so `f(f = g)` called the NEW `f` instead of loading the callee
before the args. The global fuse now requires the callee name to be
**never assigned anywhere in the script** (the assigned prepass walks
function bodies, so an uncertified function's write to the name counts)
and **no call-like node in the arguments** (a builtin like
`Object.defineProperty(globalThis, ...)` could rewrite the global callee).
A slot callee needs only the direct-arg-write check (a certified script's
args can write a declared var only directly; a frame-slot read is
side-effect-free). The 15-case probe (`scratch/callfast_slot_probe.js`)
covers the arg-writes-callee, arg-increments-callee, nested-assignment,
indirect-call-write, and getter-write ordering cases — all pass.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 5 — global-cell cache thrash (measured 2026-08-23)

The construct-churn row stayed ~74ms through every call-path slice, and
isolated measurements of the same loop showed ~36ms. The gap was the
bench itself: the 32-entry direct-mapped `global_cells` cache (Cut 5)
thrashes once a realm accumulates ~30+ globals (the bench runs each row
twice in one realm, and the host globals add more). The construct row's
`C`/`i`/`n`/`o` collided with the earlier rows' names, so every
`LoadGlobal`/`StoreGlobal`/`FastLoopHead` took the reference path
instead of the cached probe — about 2x the loop cost, ~420ns/construct
of pure cache misses. The calls row's names happened not to collide, so
it never showed the effect.

Fixing the diagnosis: `GLOBAL_CELLS` 32 -> 256 removes the thrash
(direct-mapped, so a miss still falls back to the reference path — a
bigger table just makes collisions rare). The construct row drops
74 -> ~36ms with every other row unchanged; real-world global-heavy
scripts (many evals in one realm, host globals, libraries defining many
top-level names) get the same relief.

The construct row is now ~360ns/construct: the body (`this.x = x` =
`LoadLocal` + `AssignMemberName`) was already a certified leaf running
through `run_leaf_construct` (both steps pass `steps_are_leaf` — member
steps are not excluded), so the cost is the loop + object creation +
construct dispatch, not the member write.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 6 — register member stores (measured 2026-08-23)

The register op set grows a `StoreMemberName` (the object in the
accumulator, the value a direct operand) and the lowering accepts plain
member-assign bodies (`this.x = x`, `o.x = v`), so member-store leaves —
including the construct body — run on the register executor instead of
the step path:

- **Lowering**: `Step::AssignMemberName` with a plain `=` pops the value
  and object operands, loads the object into the accumulator, and emits
  `StoreMemberName { name, value }`; `Step::SetCompletion` is skipped (the
  register path's `Empty` maps identically to the step path's `Normal` for
  leaf calls and constructs); a body ending in a store or empty now lowers
  (a fall-off completes `Empty`). Compound assigns and computed-value
  stores stay on the step path.
- **The Acc-clobber trap**: `this.y = a + b` computes the value into the
  accumulator, then the object load would overwrite it — the lowering
  rejects a `RegOperand::Acc` value (the binary ops' right-operand
  restriction). Missed it initially; `Function/S15.3.5_A3_T2`
  (`new Function("arg1,arg2", "...; this.y=arg1+arg2;...")`) wrote the
  object into `y` until the rejection was added.
- **Executor**: the op mirrors `Step::AssignMemberName` — the nullish
  check, then `assign_member` (which can run a setter — same agent-side
  machinery the step path invokes; the pushed result is discarded by the
  frame truncate). A `leaf_operand_value` helper loads the value operand
  with the `LoadReg`/`LoadContext`/`LoadPerIter`/`LoadConst` semantics
  (TDZ and transparent-env walks included).

Measured: the construct row is unchanged (~35ms — the member write was
never the bottleneck; the machinery and dispatch were, and the register
path shaves the dispatch). The win is the machinery itself: member-store
leaves (calls and constructs) now run on the register executor. The
14-case probe (`scratch/store_member_regs_probe.js`) covers plain/const/
computed-value stores, two-store bodies, store-then-return, compound
assigns (step path), captured-object stores, and the bench shape.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 7 — register computed member stores (measured 2026-08-23)

`LeafOp::StoreMemberComputed` extends the register member-store to
computed keys: `o[k] = v` (plain `=` only) lowers with the object in the
accumulator and the key + value as direct operands, and the executor
shares the step path's machinery through an extracted
`assign_computed_plain` helper — the nullish check, the fast array
element write (canonical Number index on a plain Array, skipping the
number→string→intern and [[Set]] chain work), then `to_property_key` +
`assign_member`. The `AssignMemberComputed` step's plain `=` branch now
calls the same helper, so the mirror stays exact.

A computed key or value (`RegOperand::Acc`) keeps the body on the step
path — the object load would clobber the accumulator (the same
restriction as the binary ops and `StoreMemberName`). Compound computed
assigns (`o[k] += v`) stay on the step path.

Measured: construct churn ~35 -> ~33.5ms (the machinery + the step
refactor; mostly noise at this scale). The 14-case probe
(`scratch/store_member_computed_probe.js`) covers param/const keys and
values, the array fast path, computed-key/value step-path fallbacks,
compound assigns, construct `this[K] = x`, and the hot array-fill loop.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 8 — register member reads (measured 2026-08-23)

`LeafOp::GetMemberName` / `LeafOp::GetMemberComputed` extend the register
member ops to reads: `return o.x` / `return o[k]` bodies lower with the
object in the accumulator (and the computed key as a direct operand),
and the executor shares the step path's machinery through extracted
`get_member_name` / `get_member_computed` helpers — the nullish check,
the direct-mapped member-cell cache, the fast array element read (a
canonical Number index on a plain Array), then the property-key
conversion + property machinery. Both `GetMemberName` and
`GetMemberComputed` steps now call the same helpers, so the mirror stays
exact. A computed key (`RegOperand::Acc`) keeps the body on the step
path — the object load would clobber the accumulator.

The read ops compose with the existing register ops: `return o.x + 1`
lowers to `[LoadReg, GetMemberName, BinConst, ReturnAcc]`. The 14-case
probe (`scratch/member_read_regs_probe.js`) covers named/computed/
const-key reads, the array fast path, getters, nested reads, missing
properties, nullish throws, read-then-store bodies, and the
read-compute-read shape.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 9 — register loop bodies (measured 2026-08-23)

The remaining bench rows were already on the frame path — the costs were
script-level step dispatches in the loop bodies. `Step::RunRegBody` runs a
register-lowered loop body against the current frame in one dispatch: the
ops address the frame through `frame_get`/`frame_set` (the register
ops were refactored off the explicit stack base — the leaf path resolves
via `leaf_frame_base`, the script path via the inline `Frame`), the
accumulator is saved and restored around the run (the accumulator-loop
counter), the transient stack use is truncated to the entry length, and
the body's completion is left to the loop machinery (a throwing op
propagates after the restore).

The compiler emits `RunRegBody` in the certified canonical `for` and the
for-of/in loop bodies when `lower_leaf_ops` accepts the body steps (the
value-free `ListBegin`/`ListEnd`/reset/normalize wrappers are now
skipped by the lowering too, so a block-wrapped leaf body lowers as
well). The array-iteration inner body (`n += v`) becomes `[LoadReg(n),
BinReg(Add, v), StoreReg(n)]` in one dispatch; bodies with jumps
(break/continue), `PushAcc` counter reads, or two-member-read shapes
(`o.a + o.b`) stay on the step path.

Measured: array iteration ~71 -> ~60.5ms; the other rows unchanged. The
16-case probe (`scratch/run_reg_body_probe.js`) covers the counter
preservation, non-lowering fallbacks, the throwing-body error path,
nested loops, string-append bodies, and member-store loop bodies.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 10 — fused member reads and register spills (measured 2026-08-23)

The property-access row (`n += o.a + o.b`) had a two-member-read body that
`lower_leaf_ops` rejected: the single accumulator cannot hold the three
live values (`n` must combine after `o.a + o.b`), so it ran as 10 step
dispatches per iteration. Three additions make it a six-op register body
in one `RunRegBody` dispatch:

- **Fused reads** — `LeafOp::GetMemberNameLocal { object_slot, tdz, name }`
  and `GetMemberComputedLocal { object_slot, tdz, key }` load the object
  straight from the frame slot and run the shared `get_member_name` /
  `get_member_computed` in one dispatch (the lowering fuses any `Reg`
  object operand, with the slot's `tdz` bit carried through).
- **Spills** — `LeafOp::PushAcc` pushes the live accumulator value onto
  the value stack when an op needs to overwrite it; `LeafOp::BinAccPop`
  pops it back as the left operand of a binary. The push/pop pairs
  balance inside the body and every caller truncates the stack on
  completion or error, so a spill is just one stack round-trip.
- **The frame-left combine** — `LeafOp::BinLeftReg { op, slot }` computes
  `frame[slot] op acc` for a combine whose left operand is a frame slot
  and whose right is the accumulator's live value. The slot is read at
  the combine, after the accumulator value was computed; that is safe
  only for `tdz=false` slots (the lowering rejects `tdz=true`), because a
  member read's getters cannot reach the body's own frame slots and the
  accepted shapes write no slot between the load and the combine. It also
  preserves the operand order (`n + sum`, never `sum + n`) — the string
  concat probe checks the exact value.

The new shadow-stack entry `RegOperand::Spilled` marks a value pushed by
`PushAcc`; it is consumed only by `BinAccPop` (any load of a spilled
operand rejects the body, keeping it on the step path). The binary ops
now share a `binary_inline` helper that inlines the number-number
arithmetic (`Step::BinaryImm`'s inline) to skip the `apply_binary` call.

The property body `n += o.a + o.b` lowers to `[GetMemberNameLocal(o, a),
PushAcc, GetMemberNameLocal(o, b), BinAccPop(Add), BinLeftReg(Add, n),
StoreReg(n)]` — three dispatches per iteration (body + loop head + loop
store) instead of twelve. The combine rule also admits `y + o.a` and
`n += o.a + y` (a frame-slot left with a computed right).

Measured: property access ~87 -> ~72ms; the other rows unchanged. The
18-case probe (`scratch/slice10_probe.js`) covers the sums, the string
concat order, getter mutation observed by the second read, a throwing
getter, the frame-left and frame-right combines, computed reads, chained
reads, the TDZ rejection (a `tdz=true` left operand stays on the step
path and throws before the read), nullish throws, exotic objects,
undefined operands, valueOf coercion order, accumulator-loop counter
preservation, and nested loops.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 11 — generation-validated member value cache (measured 2026-08-23)

The property row's per-read cost after slice 10 was the member-cell
re-validation: `member_cell_get` re-borrowed the property vector and
re-checked the stored key and property kind on every read (~20ns/read).
Two changes remove most of it:

- **The generation now catches in-place value updates** (`crates/crux`):
  `set_key`'s in-place write, `array_element_write`'s in-place element
  write and dense append, and `array_define_own_property`'s dense append
  (which updates `length` in place) all bump the object generation — the
  only own-property mutations that previously missed it. The write-side
  chain cache and for-of caches only ever re-validate on a mismatch, so
  the extra bumps cost misses, never correctness.
- **`member_value_cells`** — a fronting read cache of (object id, name,
  generation, value): a generation match means no own-property change
  since the read, so the value is returned with no borrow, no key/kind
  re-check. `member_cell_get` fills it on every slot-cache hit and
  `resolve_member_cell` on every resolve; the Cut 23 proto-keyed fallback
  still re-validates against the object's own vector, so fresh instances
  keep working. The `GetMemberNameLocal` op also reads its frame slot by
  reference and tries `member_cell_get` before cloning the value for the
  full fallback, skipping one refcount bump per hit.

The two-member-read property body now costs roughly the loop + two
cached clones instead of two borrow+revalidations: property access
~72 -> ~44ms (2x vs the slice-9 baseline ~87ms). The other rows are
unchanged. The 14-case probe (`scratch/slice11_probe.js`) covers in-place
updates of the cached and sibling properties, delete/defineProperty/
data-to-accessor conversions, mid-loop mutations, two-object and
five-name cache behavior, getter non-caching, array element/length
writes, and prototype-chain shadowing.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 12 — call-site leaf caches (measured 2026-08-23)

After slice 10, the remaining bench rows' cost was the CALL: the pure
loop is ~25ms/1M iterations; `n = f(n)` adds ~50ms. The per-call
machinery was dominated by the callee validation chain — the global
cell load + borrow, `callee.kind()`, the realm check, and the
`leaf_lookup` (~20ns) — plus the leaf run. Two per-call-site leaf
caches skip the chain on a hit:

- **`global_leaf_cells`** — name → the resolved `LeafEntry` for a stable
  global callee, validated by the global object's identity and
  generation (every global-object mutation bumps, slice 11). The
  compiler's never-assigned + no-call-like-args guard makes the callee
  stable for the script's duration, so the cell load, kind check, realm
  check, and lookup are skipped. The leaf-run core is extracted into
  `run_inline_leaf` (shared by `fast_call_core` and both cache-hit
  paths).
- **`slot_leaf_cells`** — frame-slot index → the resolved entry for the
  closure held there, validated by the callee's heap payload
  (`Value::heap_payload`, a new crux accessor — the raw leaked-Rc
  pointer). The cache holds the callee itself, keeping the closure's
  allocation alive, so a payload match can never be a stale address
  reuse — the cached ir + closure env are exactly the callee's. A slot
  reassigned to a different closure (or a non-function) misses on the
  payload and re-resolves.

Measured: function calls ~71 -> ~60ms, closure capture ~72 -> ~66ms;
the other rows unchanged. The 10-case probe (`scratch/slice12_probe.js`)
covers the basic sums, two closures with distinct captures, alternating
callees mid-loop, a `globalThis` reassignment invalidating the global
cache, a builtin callee through a slot, env-free and env-reading leaves,
recursion through a slot call, a non-callable slot, and two-arg leaves.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 13 — generation-validated array element and length caches (measured 2026-08-23)

The for-of fast path re-reads the array's length and each element every
step (the stock iterator semantics), each read doing a `kind()` + a
property-vector borrow + a validation. Slice 11's generation coverage
(in-place element writes, dense appends, and the length updates all
bump) makes both reads cacheable by generation:

- **`array_element_value_cells`** — (array id, index, generation, value)
  fronts `array_element_get`: a generation match returns the element with
  no borrow or key/kind re-check. `resolve_array_element` fills it on
  every resolve, and the computed member-read path (`a[i]`) shares the
  same fast read.
- **`array_length_cells`** — (array id, generation, length) fronts
  `array_length`: a generation match skips the borrow and the number
  conversion the for-of head pays every step.

Measured: array iteration ~58 -> ~44ms; the other rows unchanged. The
10-case probe (`scratch/slice13_probe.js`) covers the sums, in-place
and push mutations observed by the same loop, direct length sets,
holes, two-array cache separation, `a[i]` computed reads, mutations
between passes, truncate-and-grow, and sparse arrays.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 14 — fused leaf binary operands (measured 2026-08-23)

Two more leaf-op fusions cut the leaf bodies' op count:

- **`BinImmLocal { op, slot, tdz, imm }`** — `return x + 1` fuses the
  `LoadReg` + `BinImm` pair into one dispatch (the frame-slot read with
  its `tdz` check, then the number-number inline).
- **`BinCtxReg { op, index, slot, tdz }`** — `(y) => x + y` fuses the
  `LoadContext` + `BinReg` pair (the context-transparent env walk, then
  the frame-slot read — the two reads have no side effects between
  them, so the evaluation order is unchanged). The lowering emits it
  for a captured left with a frame-slot right; the `Binary` arm's
  `else` branch is now a `match (left, right)` with the fused case
  first.

The calls and closure leaf bodies drop from three ops to two
(`BinImmLocal`+`ReturnAcc` / `BinCtxReg`+`ReturnAcc`). Measured mins
(heavy machine load during this slice made the full-suite medians
unreliable; the load inflates the memory-heavy rows): function calls
~60 -> ~59ms, closure capture ~66 -> ~65ms; the other rows unchanged.
The 12-case probe (`scratch/slice14_probe.js`) covers the number and
string operands, valueOf coercion order, the context-transparent walk,
per-iteration + param shapes, non-fused two-param leaves, TDZ left
operands, and the mul/sub/div immediate forms.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 15 — construct-inline lookup through the leaf cache (measured 2026-08-23)

The `Construct` step's certified fast path looked up the callee in the
`ecma_functions` HashMap on every construct — unlike the call path,
which reads a direct-mapped leaf cache. Since a construct-inline body
is a leaf (the `construct_inline` verdict — leaf body, base kind, no
fields/private methods — is cached at ir-compile time), the shared
[`LeafEntry`] now carries that flag, `leaf_lookup` captures it from the
record, and the `Construct` handler reads the leaf cache like
`do_call_fast` — one direct-mapped probe instead of a HashMap lookup
per construct. Non-construct-inline callees (arrows, classes, derived
constructors) fall through to the general machinery unchanged.

Measured (heavy machine load through this slice — the memory-heavy
rows are inflated and the construct win is at the noise floor):
construct churn ~36ms, ~1ms from the lookup removal. The 10-case probe
(`scratch/slice15_probe.js`) covers the bench sum, object-return wins,
primitive-return fallbacks, non-leaf and captured-env constructors,
arrows (TypeError), classes, derived classes with super, new.target,
and prototype-chain instanceof.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 16 — register-encode the accumulator-loop counter read (measured 2026-08-23)

The arithmetic row (`n += i * 2`) was the last step-path bench: its body
reads the accumulator-loop counter (`Step::PushAcc`), which the register
model could not express, so it ran as 5 body steps + the loop head per
iteration. The counter read now lowers:

- **`RunRegBody { ops, push_counter }`** — when the body reads the
  counter, the saved counter is pushed onto the value stack at entry.
- **`LeafOp::LoadCounter`** — acc = pop() (the pushed counter).
- The lowering maps `Step::PushAcc` to a `RegOperand::Counter` shadow
  entry consumed by `load_operand` (at most one per body — the counter
  is pushed once; a second read keeps the step path). A counter as a
  store key/value or a binary right operand is rejected (the operand
  loader cannot express a pop there).

The arithmetic body becomes `[LoadCounter, BinImm, BinLeftReg,
StoreReg]` in one dispatch: arithmetic ~37.5 -> ~33ms. The 12-case probe
(`scratch/slice16_probe.js`) covers the bench sum, the counter as a
store/right-operand (step-path fallbacks), float counters, the
loop-exit counter value, nested loops, two-read bodies, factorial,
string concat, countdowns, and break/continue.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 17 — fused slot-callee call-store (measured 2026-08-23)

The calls/closure rows' loop bodies were `LoadLocal(arg) + CallFastSlot +
FusedStoreLocal` — three dispatches plus the argument and result stack
round-trips per iteration. `emit_statement_store` now recognizes the tail
pattern (the args' `LoadLocal`s followed by a real `CallFastSlot` — whose
guards already passed — and a slot store) and replaces it with
`Step::CallFastSlotStore { callee_slot, arg_slots, store_slot }`: the arg
slots are read in order with their `LoadLocal` TDZ checks, the
slot-callee call runs through the existing machinery (the transient arg
push is truncated by the call core), and the result stores with the
`FusedStoreLocal` TDZ check. The pattern only matches plain slot args, so
member/nested/compound shapes keep the step path unchanged.

The `n = f(n)` body becomes one dispatch: function calls ~59 -> ~57ms,
closure capture ~65 -> ~60ms. The 14-case probe
(`scratch/slice17_probe.js`) covers the sums, 0/1/2-arg shapes, the
arg == store-slot order, member and global callees (no fusion), nested
calls, compound assigns, expression-position assigns, TDZ targets and
args, throwing and non-callable callees.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 18 — do-while for-of/for-in loop shape (measured 2026-08-23)

The certified for-of loop was `[ForOfNextBindLocal; body; Jump(top)]` —
three dispatches per element, the back-jump being one of them. The
protocol steps gained a `back` target and the loop is now a do-while:

```
34: ForOfNextBindLocal { slot: 2, done: 39, back: 35 }   // prologue fetch
35: RunRegBody { ops: [...], push_counter: false }         // body start
36: ForOfNextBindLocal { slot: 2, done: 39, back: 35 }   // loop-bottom fetch
37: ForOfClose
...
39: NormalizeCompletion                                  // done
```

The per-iteration fetch at the loop bottom **is** the back edge: on a
successful fetch `ip = back` (the head bind / body start), so a
straight-line body has no per-element `Jump` dispatch. The prologue
fetch and the loop-bottom fetch are separate steps with the same
`done`/`back`; `continue` targets the per-iteration copy (captured
heads) or the loop-bottom fetch itself, exactly the old back-edge
ordering. The generic (uncertified) paths keep the `Jump` — their
`back` is just the next step (no behavior change) — and the for-of/
for-in `ForOfBoundary` span is unchanged.

The array-iteration row drops ~2-8ms (interleaved same-load baseline,
median ~-5) with every other row flat: the array bench inner loop is
now 2 dispatches per element (fetch + `RunRegBody`). The 30-case probe
(`scratch/slice18_probe.js`) covers dense/hole/sparse arrays, continue,
break, labeled break, per-iteration captures with continue/break,
nested for-of, bodies with calls, generic iterators (Set/string),
for-in with deletion and continue, array mutation during iteration,
zero-iteration loops, and the uncertified paths (destructuring heads,
`with`-forced, lexical-head restore).

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 440 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 20 — fused global-callee call-store (measured 2026-08-23)

The calls row's statement-position call was the one unfused call shape:
`n = f(n)` with `f` a global function declaration compiled to
`ListBegin; LoadLocal; CallFastGlobal; FusedStoreLocal; ListEnd` — the
slice-17 slot-callee fusion skipped global callees. `emit_statement_store`
now recognizes the same tail shape with a `CallFastGlobal` (the slice-2
fused call, `direct_eval` always false) and emits
`CallFastGlobalStore { name, arg_slots, store_slot }` — the arg loads, the
global-callee call (through the slice-12 global leaf cache), and the slot
store in one step, mirroring `CallFastSlotStore` exactly (arg TDZ checks,
the store's `FusedStoreLocal` TDZ check).

Measured: ~0-2ms on the calls row (alternating-order A/B, 5M-iteration
isolation — the engine's dispatch is jump-table cheap, so the 2 saved
dispatches sit below the noise floor). The change is a structural
unification (both callee kinds now fuse) with zero regression. The 16-case
probe (`scratch/slice20_probe.js`) covers 0/1/2-arg global callees,
arg==store-slot order, two-slot shapes, member/compound/expression-position
fallbacks, TDZ args/targets, throwing callees, multiple call sites, and
the slot-callee path (unchanged).

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 21 — dedicated loop-counter field (measured 2026-08-23)

The accumulator-path fast loop kept its counter in `Vm::acc`, so every
register-lowered loop body had to save and restore the accumulator and,
when the body read the counter, push the counter onto the value stack for
`LoadCounter` to pop. The counter now lives in a dedicated
`Vm::loop_counter` field: `FastLoopBind`/`FastLoopHead`/`FastLoopStore`
and the `PushAcc`/`PopAcc`/`IncAcc`/`DecAcc` steps read/write the field
(`FastLoopVar::Acc` is renamed `Counter`), `RunRegBody` no longer
saves/restores the accumulator or pushes the counter (its ops clobber the
accumulator freely), and `LoadCounter` reads the field directly — the
`push_counter` machinery is gone.

**The containment point is `run_leaf_body`**: a leaf-inline body can itself
contain a fast loop (`steps_are_leaf` allows the fast-loop steps), so the
caller's counter is saved/restored across the leaf run exactly like the
accumulator — without it, a leaf's `FastLoopBind` would clobber the
caller's live counter.

Measured: ~0-2ms on the counter-reading rows (alternating-order min-of-3
isolation — the removed save/restore + push/pop sit near the noise floor;
full-bench medians are flat). The win is structural: the counter never
round-trips the value stack, and the field is the prerequisite for a
raw-f64 counter (removing the head's Value ops). The 18-case probe
(`scratch/slice21_probe.js`) covers the bench shape, countdowns,
break/continue, nested loops, leaf-with-loop containment (including a
throwing leaf and two-level nesting), string counters, counter
writes/incs in the body, the for-of inner body reading the outer counter,
and closure bodies.

Validation: clippy `-D warnings` clean, workspace tests green (4312/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 440 >15s hangs in the known slow classes (unchanged).

### Cut 35 slice 22 — raw-f64 loop counter (measured 2026-08-23)

The `Vm::loop_counter` field is now an `f64` instead of a `Value`. The
loop head's per-iteration work was the Value round-trip — clone the
counter, `as_number()`, compare for the test; clone, `as_number()`,
`Value::Number(x + delta)` for the increment — and the body's
`LoadCounter` wrapped the field back into a Value. With the raw float the
test is a direct f64 compare (NaN semantics match JS: both compare
false), the increment is `self.loop_counter += delta`, and `LoadCounter`/
`PushAcc`/`FastLoopStore` wrap once per read/write instead of per head
dispatch.

**Soundness — the acc-path gates must admit only Numbers.** An f64 cannot
hold a String/BigInt, and the loop protocol lets a non-Number counter
(which the old Value field kept verbatim) flow through `FastLoopBind`
(the init) and `PopAcc` (a statement-position `counter = expr`). Two new
compile gates restrict the acc path: `for_init_counter_number` requires
the loop init to be a provably-Number expression (a numeric literal,
`+expr` — always Number or throws before the store — or `-expr` on a
provably-Number operand, excluding BigInts; a missing/expression/multi-
decl head with a non-Number last counter initializer is rejected), and
`acc_expr_safe`'s statement-position `counter = expr` case now requires a
provably-Number RHS. Everything rejected falls back to the fused slot
path, which is behaviorally identical (it coerces via the general
machinery) — the gates are a pure eligibility restriction, never a
semantics change. The runtime keeps `debug_assert!`s on the gated
bind/store conversions.

The leaf containment point is unchanged: `run_leaf_body` saves/restores
the field across a leaf run (now a plain f64 copy). `Vm::new`/`reset`
seed the field with `0.0`; no code reads it outside an active fast loop.

Measured: a real win — the first structural lever to move since the
step-fusion floor. 5M-iteration alternating-order min-of-3 isolation
(14 runs per binary, base = the slice-21 tree at `89bba88`): empty-head
loop 59→35ms, counter-read body 166→132ms, arith row 165→122ms
(≈5-9ns/iter off the head and counter read; consistent across both
pair orders, far above the ±2-5ms order bias). The 25-case probe
(`scratch/slice22_probe.js`) covers the bench shapes, countdowns,
break/continue, nested loops, leaf containment (including a throwing
leaf), the Number-literal/write gates, and the slot-path fallbacks for
String/BigInt inits and writes (all behaviorally identical).

Validation: clippy `-D warnings` clean, workspace tests green (4313/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs (all in the known slow classes;
7 property-escapes more than slice 21's 440, 1 less non-whitespace —
load-classification wobble, no added fail/crash paths).

### Cut 35 slice 23 — caller-frame argument aliasing (measured 2026-08-24)

The fused `CallFastGlobalStore`/`CallFastSlotStore` steps (slices 17/20)
pushed each argument from a caller frame slot onto the value stack, then
the leaf-inline callee either re-read it from that stack region (`Alias`)
or copied it into a pushed frame (`Pushed`). The argument round-trip —
clone + Vec push, stack read, pop — was the fused call core's largest
remaining per-call cost after slice 22.

Slice 23 reads the arguments straight from the caller's frame slots. A
register leaf whose frame is exactly its parameters — `frame_size ==
arity`, no `this`/`arguments`/`var` slots, and no parameter is ever
assigned (the new `ScopeInfo::args_alias` gate, computed in
`analyze_scope` via the assigned-name collection, which walks nested
closures too) — runs with a new `LeafFrame::CallerSlots { base }`: the
caller's `leaf_frame_base` stays active, a new `Vm::leaf_frame_offset`
addresses its slots directly, and nothing is pushed or unwound. The
fused steps keep their TDZ checks (moved ahead of the call, no push) and
pass the args' base; the call core uses the aliasing on a cache hit and
materializes the args on the stack on any fallback (param write, var
slot, `this`, `arguments`, a step-path leaf, a non-leaf — behavior
identical to the pre-slice path).

**The debug-stack trap**: the first cut inlined the CallerSlots run as a
second `run_leaf_regs` call site inside `run_inline_leaf`, and with
`#[inline(always)]` the whole register dispatcher got duplicated into
`fast_call_core`'s per-recursion-level debug frame — a step-path leaf
recursion (the `fast_path_function_declarations` test's `g(5)`) that
passed at the default test-thread stack in the base now overflowed
(needed `RUST_MIN_STACK` 4MB). `run_inline_leaf` was restructured to a
single `run_leaf_regs` call site (the frame source is decided first, one
result-placement tail with a `pre_call` base) — the test passes at the
default stack again and the release win is unchanged.

Measured — the call rows drop ~12-16%: a release Rust-side timing probe
(min-of-5 evals of the exact bench sources, same agent, identical in
both worktrees) shows `n = f(n)` 5M at 254→223ms and the closure-capture
row at 303→253ms (~6.4ns/call and ~9.9ns/call off the fused call core).
The `--bench` harness confirms the direction but swings with load.

The 13-case probe (`scratch/slice23_probe.js`) covers the bench shapes,
two-param leaves, param-write/var-slot/`this`/step-path/non-leaf
fallbacks, the closure-capture shape, extra args, a `let` TDZ arg, and
an arg slot distinct from the target slot. New unit tests assert
`ScopeInfo::args_alias` across the gate (including a closure writing a
captured param) and the fused-site behavior.

Validation: clippy `-D warnings` clean, workspace tests green (4315/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs (identical set to slice 22).

### Cut 35 slice 24 — result-store direct write (measured 2026-08-24, REVERTED)

The other half of the fused call round trip: `run_inline_leaf` wrote the
leaf's result straight to the target slot (`result_target` + a `bool`
return so the caller skipped its pop) instead of the `stack.push` →
handler `pop()` → store. Measured a ~1.3ns/call REGRESSION on both call
rows (5M-iteration probe, consistent in both pair orders): the
amortized Vec push+pop cost less than the plumbing (the param, the bool,
and the store's TDZ check moved into the inlined tail) needed to remove
them. Reverted before commit — the result round trip is not a lever.

### Cut 35 slice 25 — leaf-call core plumbing (measured 2026-08-24)

Decomposed the ~17ns leaf-call core (5M-iteration probe: `z() { return
1; }` 40.4ns/call minus the 23.7ns no-call loop) into the body ops
(~3ns each) and the machinery, then shaved four pieces:

- **`global_matches`** — the global-leaf-cache validation read the cached
  global handle in place instead of `global_object`'s per-call Rc clone.
- **`bind_this_value` skipped for no-`this`-slot leaves** — the common
  leaf's call now returns `undefined` without the call.
- **`Agent::realm_count`** — the `realms.borrow().len() == 1` check (five
  call sites) is a plain `Cell<usize>`, exact because `realms` is only
  ever pushed (via `initialize_host_defined_realm`) and never popped.
- **The accumulator save/restore removed from `run_leaf_regs` and
  `run_leaf_body`** — `Vm::acc` is read only by the register executor,
  and every leaf-inline call site sits in a step-path body where the
  caller's `acc` is dead (the step path never reads it; the leaf's first
  op loads the accumulator from scratch). The loop-counter save stays
  (a leaf's own fast loop must not clobber the caller's live counter).

Measured: ~-1.6ns/call on both the zero-arg and `n = f(n)` rows
(5M-iteration probe, alternating both pair orders: z 198→190ms, f1
228→220ms) — the first core-plumbing win. The remaining core cost is
`run_leaf_ops`' dispatches (the actual body work) plus the frame/
completion/env saves, which are required.

Validation: clippy `-D warnings` clean, workspace tests green (4316/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs (identical set to slice 23 — the acc
removal is a semantic claim the full sweep backs).

### Cut 35 slice 26 — inline number-number `Add`/`Exp` (measured 2026-08-24)

The register-op dispatch (`run_leaf_ops`) and the step path's
`BinaryImm` both inline the number-number arithmetic shape for
`Sub`/`Mul`/`Div`/`Rem` — but not `Add` or `Exp`, so the most common
combine (`x + 1`, and every acc-combine `(x + 1) + 1` chain) fell to
the general `apply_binary` call. The `binary_inline` helper (the fused
`BinImmLocal`/`BinCtxReg`/`BinAccPop` shapes) already had `Add` — the
acc-combine arms were inconsistent. All four inline sites now cover the
full arithmetic set (`Add`/`Sub`/`Mul`/`Div`/`Rem`/`Exp`); `apply_binary`
for two numbers is a plain float op, so the inlines are exact, and the
captured/per-iteration binary arms (`BinContext`/`BinPerIter`) now route
through `binary_inline` for the same reason.

Measured with a body-op slope (5M-iteration probe, `return x + 1 + 1 +
…` with increasing `+1` counts): each acc-combine `+1` op dropped from
~7ns to ~3.7ns — the ops2/ops3/ops4 rows fell 249→229, 283→248,
323→266ms. The bench rows do not move (they already use the inlined
fused shapes — `i * 2` is `Mul`, `n + i*2` and `o.a + o.b` combine via
`binary_inline`); the win is the generic `+`/`**` arithmetic bodies.

Validation: clippy `-D warnings` clean, workspace tests green (4315/0),
conformance language 23,724/0/34, annexB 1,086/0/0, built-ins
23,812/0/154 with 447 >15s hangs (identical set to slice 25).

### Cut 35 slice 27 — micro-benchmark vs Node re-measurement (measured 2026-08-24)

Re-ran the Cut 11 comparison against Node v24.12.0 (same sources, same
warmup-then-time-2nd-run methodology, all 8 rows). Node runs each row in
a clean per-row context (`new Function`, warmup call then timed call —
`scratch/bench_node2.js`); that is what reproduces Cut 11's node numbers
(arith 10.2/11ms, array 24.9/26ms, calls 16.1/16ms jitless). Medians of 3
interleaved runs of each binary (order rotated per round to cancel the
machine drift):

| Benchmark | slag | node (jit) | node (--jitless) | slag vs jitless |
|---|---|---|---|---|
| arithmetic | 26.1ms | 0.6ms | 10.4ms | 2.5x |
| property access | 57.8ms | 0.3ms | 14.9ms | 3.9x |
| string concat | 18.4ms | 0.5ms | 1.4ms | 13.3x |
| array iteration | 52.5ms | 0.6ms | 24.6ms | 2.1x |
| function calls | 44.3ms | 1.1ms | 18.6ms | 2.4x |
| closure capture | 52.9ms | 0.9ms | 18.6ms | 2.8x |
| per-iteration | 16.9ms | 0.2ms | 2.5ms | 6.8x |
| construct churn | 38.6ms | 1.7ms | 3.0ms | 12.9x |

All 8 rows report `ok=true` on both engines. Since Cut 11 (measured
2026-08-21, jitless ratios 12-40x) the interpreter core has closed most
of the gap: function calls 2.4x (the old 40x standout, closed by the
caller-frame arg reads of slice 23 and the leaf-call plumbing shave of
slice 25), arithmetic 2.5x, closure capture 2.8x, property access 3.9x,
array iteration 2.1x — the closest row. The remaining gaps are the
non-core machinery: string concat 13.3x and construct churn 12.9x lead
(node's cons-string ropes and fast allocation are hard interpreter
targets), then per-iteration 6.8x (closure call/env reads).

Harness trap: an earlier attempt eval'd each source in a shared global
scope (mirroring how the slag `--bench` rows share one context) and
measured node array iteration at 62ms jit / 88ms jitless — "beating"
node's JIT with slag's 52ms. That under-states V8: the timed 2nd eval is
a fresh compilation TurboFan has not finished tiering, and the
accumulated global `var`s push the global object to dictionary mode (the
same shared-context artifact class as slag's own slice-5 cache thrash).
Cut 11's node numbers reproduce only with the clean per-row harness, so
that is the comparison used here. Note the asymmetry: slag's `--bench`
keeps all 8 rows in one context while node is per-row clean; the
`--jitless` column is the design-relevant interp-vs-interp picture.

### Cut 35 slice 28 — rope appends: iterative drop, Arc-shared flats, higher fold cap (measured 2026-08-24)

The string-concat row builds a 100k-unit rope via `s += 'x'`. Its cost
split into the append machinery (an `Rc<JsString>` wrapper + rope node +
right-operand box per append) and the depth-cap flatten: `concat` folded
the *entire accumulated left side* into a fresh flat every
`ROPE_MAX_DEPTH` (1024) appends, copying ~n²/(2·cap) units — ~10 MB at
100k, ~1 GB at 1M (the 1M probe took ~480 ms, ~3× the machinery floor).
The cap existed only to bound drop recursion.

Three changes to `crates/crux/src/string.rs`:

- **Iterative drop.** `RopeNode` children are now `Option<JsString>` and
  `Drop` unwinds the tree with an explicit worklist
  (`Arc::try_unwrap` on uniquely-owned subtrees, decrementing shared
  ones), so arbitrarily deep chains — including the right-leaning
  prepend chains the cap never protected — free without stack recursion.
- **Arc-shared flat buffers.** `Flat(Arc<[u16]>)` makes `JsString` clones
  O(1): the rope's per-append `right` operand (a 1-unit box every
  iteration) is now an Arc bump, and the depth fold shares the
  materialized flat instead of copying it a second time.
- **Higher fold cap (1024 → 16384).** The cap now exists purely for the
  amortized fold-vs-drop tradeoff: folding every 16384 appends copies
  ~n²/32768 units (~0.6 MB at 100k, ~60 MB at 1M) and bounds the final
  tree to ~cap nodes, so dropping a 100k chain went from ~7 ms (100k
  individual node frees, the uncapped version) to <1 ms.

Measured (release, 3-run interleaved medians vs the slice-26 base
binary):

| Row | before | after |
|---|---|---|
| string concat | 18.6ms | **16.7ms** (-10%) |
| string concat 1M probe | ~480ms | **~230ms** (2.1x) |

All other bench rows flat (property-access +3.4% and per-iteration +2.8%
were within the ±5% drift band — a tight interleaved re-check of
property access showed no regression). Semantics are unchanged (strings
are immutable; the fold/flatten are internal representation), but the
shared rope machinery warranted the full sweep: language 23,724/0/34,
annexB 1,086/0/0, built-ins 23,812/0/154 with 447 >15s hangs — identical
to baseline. Workspace tests 4316/0, clippy `-D warnings` clean.

The remaining gap vs node jitless (1.4ms → ~12x) is the append machinery
itself (~165ns/iter): the per-append `Rc<JsString>` wrapper and node
allocation on top of the VM's leaf-register body. The structural next
lever is a `Value::String` representation that avoids the per-append Rc
box.

### Cut 35 slice 29 — inline the string-string concat in `binary_inline` (measured 2026-08-24)

A leaf-path decomposition of the string row (1M-iteration probes): the
loop/register machinery is ~30ns/iter, the VM's concat path (the
`apply_binary` call + two `as_string` Rc round-trips + the result
`Handle`) added ~28ns, and the rope concat itself is ~57ns base plus up
to ~50ns of fold overhead at the 100k bench scale. Slice 29 removes the
VM-side call and Rc churn:

- `binary_inline` now inlines the (String, String) `Add` case — the rope
  concat — skipping the `apply_binary` call (the number-number inline
  from slices 10/26 was already there; this is the string counterpart).
- New `Value::as_string_ref` borrows the string without `as_string`'s Rc
  reconstruct-and-clone round-trip; `apply_binary`'s string fast path and
  the new inline both use it.
- `LeafOp::BinConst` now routes through `binary_inline` (replacing its
  duplicated number inline), so the bench body's `s += 'x'` (LoadReg,
  BinConst String(x), StoreReg) takes the string fast path.

Measured (1M-iteration leaf probes, interleaved base/new): the
empty-append probe (`s += ''`, the is-empty fast path — no node build)
went 63 → ~56ms (~7ns/iter: the call + Rc round-trips removed), and the
and the full-append probe's min went 191 → 182ms. The bench row moved 16.7 →
~16.5ms (within the ±5% drift band — the isolated probes carry the
signal). Validation: clippy `-D warnings` clean, workspace tests 4316/0,
full sweeps at baseline (language 23,724/0/34, annexB 1,086/0/0,
built-ins 23,812/0/154 + 447 hangs).

### Cut 35 slice 30 — merge the rope node into the string box (measured 2026-08-24)

`JsString` becomes a single enum: `Flat(Arc<[u16]>)` or
`Rope { left/right: Option<Handle<JsString>>, len, depth: u32, flat }`.
The rope node IS the box the value points at, so an append is **one
allocation** (was: an `Rc<JsString>` wrapper + an `Arc<RopeNode>`), and
`concat` now takes `&Handle<JsString>` and returns `Handle<JsString>` —
the operands' own boxes become the node's children (Rc bumps, no
copies). The empty-operand paths return the operand handle with no
allocation at all. Sizes: the box went 16 → 48 bytes (enum + u32 depth),
so each append allocates 64B instead of ~144B across two boxes.

Consequences handled:

- **`!Send`.** Rc children make `JsString` non-`Send`; the only `Send`
  consumer was the well-known-symbol table, which is now `thread_local`
  (per-agent — the spec wants per-realm symbols anyway).
- **Iterative drop.** The `Drop` impl `take`s the children (Rc has a null
  niche, so `Option<Handle>` is still 8 bytes) and unwraps uniquely-
  owned subtrees with a worklist; cloning children instead of taking
  them silently degraded to recursive field-drop deallocation (the first
  cut overflowed the stack at 200k nodes — the take-based version is
  what ships).
- **Flat-cache sharing lost.** Rope clones get fresh `flat` caches (a
  shared rope flattened through two handles flattens twice); the
  `rope_equality_and_clone_share_the_flat_cache` test became
  `rope_equality_and_clone_correctness` (content, not pointer,
  equality).
- **`large_enum_variant` lints.** The 16 → 48B `JsString` grew every AST
  node embedding it, crossing clippy's threshold on `ExportDecl`
  (`crates/syntax/src/ast.rs`) and `StaticElement` (`class.rs`) — both
  got targeted `#[allow]`s with comments (boxing the AST strings is
  deferred; the AST is transient compiler input).

Measured (release, interleaved 3-run medians vs the slice-29 binary):

| Row | before | after |
|---|---|---|
| string concat | 17.6ms | **12.0ms** (-32%) |
| string concat 1M probe | ~210ms | **~132ms** (-37%) |
| empty-append 1M probe | ~56ms | **~38ms** (-32%) |

All other bench rows within ±2.3% (function calls +2.3%, closure
capture +1.6% — a residual code-layout cost of the bigger string code;
per-iteration -6.8% was within noise). One trap surfaced and fixed
during measurement: `binary_inline`'s string-string `Add` inline
**bloated every register-op call site's icache** (the concat body is
large) — the call and closure rows measured +3-6ns/call until the
string path moved to a `#[inline(never)]` `concat_strings` helper. The
slice-29 `Value::as_string_ref` borrow is gone (dead): the Handle-based
`concat` needs the `as_string` handles.

Validation: clippy `-D warnings` clean (the two allows above),
workspace tests 4316/0, full sweeps at baseline (language 23,724/0/34,
annexB 1,086/0/0, built-ins 23,812/0/154 + 447 hangs). The remaining
string-concat gap vs node jitless (1.4ms → ~8.6x) is the loop machinery
plus the allocation itself — the next lever is the arena/GC milestone.

### Interpreter per-op floor (2026-08-31 measurement, resolved 2026-09-01)

The 2026-08-31 profiling of the sweep's slowest cluster (the RegExp
property-escapes / CharacterClassEscapes fixtures, ~380-430 load-dependent
"hangs") traced the cost to the interpreter's per-op speed: the vendored
harness `buildString` (`test262/harness/regExpUtils.js`) fills a JS array
in a loop and calls `String.fromCodePoint.apply`, and that body ran
entirely interpreted:

| primitive | 2026-08-31 | now (2026-09-01) | node (approx.) |
|---|---|---|---|
| bare `for` iteration (1M) | ~0.7 µs/iter | ~14.4 ns/iter (~49x) | 1-10 ns |
| indexed store `a[l++] = v` (1M) | ~2.8 µs/op | ~48 ns/op (~58x) | ~5-20 ns |
| buildString shape (3M appends) | ~1670 ms | ~180 ms (~9x) | ~6 ms |

Both levers the 2026-08-31 section recommended are resolved:

1. **JIT certification for the `buildString` shape is done** (2026-09-01):
   dense element stores and the array-store / member-write / `.apply`
   steps certify — the JIT rows are buildString shape ~43ms / full ~38ms
   under the default CLI JIT (see the CLI-JIT-default section), and the
   apply/call builtins leaf-inline certified-leaf callees (see the
   vector-call/apply section). The hang cluster is gone — current full
   sweeps report 0 hang.
2. **The interpreter's core loop itself is ~50x better on the same
   shapes.** The bare loop is ~14.4ns/iter and an indexed store ~48ns/op
   (the cuts since then: fused loop heads, register bodies, the raw-f64
   loop counter, the generation-validated element caches, and the
   allocation-free element encode). The `--bench` rows `bare loop` and
   `indexed store` keep the floor visible, per the original section's
   suggestion. The residual gap to mainstream on the store shapes is the
   per-op helper/FFI cost and the property-vector writes — a real but
   secondary target, as the original note said.

### CLI JIT default, detached-view length, and the typed-array JIT picture (measured 2026-09-01)

Supersedes the "`--jit` changes nothing on these loops" claim in the floor
section above: dense element stores (609ce62) and the `buildString` shape now
certify (b61a0b5), so the JIT is a real escape hatch for that cluster
(`--jit-bench` buildString rows: 191ms → 43ms).

- **The CLI installs the JIT by default** (2026-09-01): `slag file.js` runs
  certified bodies through Cranelift; `--no-jit` opts out; `--bench` still
  measures the interpreter floor. Default vs `--no-jit` on the Node probes:
  buildString shape 43ms vs 194ms (4.5x), buildString full 29ms vs 113ms
  (3.9x).
- **The numeric-index typed-array `length` fast path missed the
  detached-buffer branch**: `typed_array_effective_length` returned the
  pre-detach length after `transfer()` (the spec's length getter returns 0
  for a detached view, 25.2.3.1). Fixed at the source — the function now
  returns 0 for a detached buffer, which also fixes
  `typed_array_own_property_keys` on detached views.
- **The "JIT ignores typed-array loops" reading was a probe artifact**: a
  `new Uint8Array(...)` inside the body emits `Step::Construct`, which the
  JIT cannot lower, so the whole body bails to the interpreter. With the
  array hoisted (passed in), the same loops compile: `view.length` loop
  56ms → 18ms (3.1x), element-write loop 92ms → 54ms (1.7x). Native `fill`
  stays ~700ms with or without the JIT — the builtin pays a per-element
  `Vec` encode/decode allocation.

Node v24.12.0 comparison (same machine, interleaved 7-rep medians):

| probe | node | slag default (jit) | slag --no-jit | gap |
|---|---|---|---|---|
| buildString shape (3M appends) | 6ms | 42ms | 189ms | 7x |
| buildString full (2.16M cps) | 13ms | 28ms | 110ms | 2.2x |
| typed-array length read (800k) | ~1ms | 56ms | 56ms | ~56x |
| typed-array element write (800k) | ~1ms | 90ms | 90ms | ~90x |
| typed-array native `fill` (800k) | <1ms | 680ms | 680ms | ~680x |

### Typed-array fill fast path, the JIT inline typed-array store, and the remaining typed-array picture (measured 2026-09-01)

The typed-array rows above were the largest remaining gaps; two fixes closed
most of them, and the CLI's `--jit-bench` gained permanent rows (typed-array
write / typed-array length) so the floors stay visible. Note the table's
56ms/90ms columns for the length/write rows were the INTERPRETER numbers —
the hoisted-array probes compile (see the probe-artifact bullet above) and
measure 19.5ms / 51.5ms under the default JIT.

- **`fill` encodes once and writes the buffer directly.** The old loop ran
  `set_property(this, &key(k), value)` per element: a decimal-string
  `JsString` allocation, a `canonical_index` parse back to an index, and a
  fresh `encode_element` `Vec` per write. The builtin already coerces the
  value exactly once (spec 25.2.3.9 step 4) and re-validates the view after
  the coercion (steps 12-13), so the new path encodes once and writes the
  same bytes per element — `JsObject::typed_array_fill_encoded`, no
  per-element key, parse, or allocation. Measured in the release unit
  harness (5 fills of 800k, ~1.97ms each): **~680ms → ~2ms (~340x)**, i.e.
  the ~680x row is now ~2x vs node.
- **The JIT's inline store helper now handles typed arrays.**
  `fast_array_element_write` (the compiled `o[i] = v` fast path) previously
  covered only plain Arrays; typed arrays fell through to the general
  `assign_member_computed` helper — a second FFI call plus re-checks per
  element. It now stores through `typed_array_element_set` for
  `IntegerIndexed` receivers, gated on PRIMITIVE values only: an
  Object/Function value's element coercion (ToNumber/ToBigInt) can run
  `toPrimitive` user code, which the helper's "never sets the pending byte"
  contract forbids, so those return 0 and the fallback runs the identical
  coercion on the VM (nothing observable ran on the fast attempt, so
  re-running is safe; the wrong-content-type TypeError is thrown by the
  fallback). A/B on the new bench row (3 runs each, alternating order): the
  JIT write row dropped **~59ms → ~51.5ms (~13%)**, non-overlapping spreads.
- **The own-`length` divergence is fixed** (2026-09-01, `58b6763`): a
  typed array CAN carry an own `length` data property —
  `Object.defineProperty(ta, 'length', {value: 1})` succeeds via
  OrdinaryDefineOwnProperty — and it must shadow the prototype accessor
  (node reads 1), but the interpreter's `get_member_name` typed-array
  shortcut returned the slots length first (slag read 4). The shortcut is
  now gated on the absence of an own `length`
  (`JsObject::has_own_property_atom` — a parse-free vector scan, usually
  empty; measured ~6% on the JIT length row, within noise), falling
  through to OrdinaryGet when one exists; a configurable delete restores
  the accessor. Every read path shares the helper, so the JIT is covered
  transitively. Verified against node v24.12.0; unit tests cover
  data/accessor shadowing, redefine, delete, and unrelated own
  properties; the full sweep is green. The JIT `view.length` helper
  (below) is now safe to build.

Remaining levers (the two typed-array rows below are closed — see the
next section), in order of expected value:

| lever | current gap vs node | where |
|---|---|---|
| GC slot-arena allocation (GC-5's remaining lever) — `Gc::new` heavier than `Rc::new`; recovers the construct-churn / string-concat ~2x regressions | 2x | `docs/gc-plan.md` |
| interpreter per-op floor — the 0.7µs bare-loop iteration is ~100x off mainstream; the floor section calls the VM core a real but secondary target | ~100x | `runtime/src/ir.rs` |
| the apply floor — **closed** (the compiled `CallApply` step below): interp 208→101ms, jit 204→89ms on the `apply leaf call` row; the residual is the member read + element reads + leaf frame setup | ~5x on a 1-elem apply | `runtime/src/ir.rs` |

### Typed-array element encode and the compiled length probe (measured 2026-09-01)

The two remaining typed-array levers are closed; the typed-array rows are
now the closest to node in the whole suite.

- **The per-element encode no longer allocates.** `encode_element`'s
  fresh `Vec<u8>` per write is replaced by `encode_element_into` — a
  stack `[u8; 8]` buffer (the largest element is 8 bytes) returning the
  used length — used by every per-element write path
  (`typed_array_element_set`, `typed_array_set`, the index define),
  plus Atomics' `element_raw` and `fill`'s encode-once. The JIT store
  helper inherits it through `typed_array_element_set`. A/B on the
  `--jit-bench` rows: the JIT write row **51.5ms → ~30ms (~42%)** and
  the interpreter write row **~92ms → ~73ms (~20%)** (3 runs each,
  stable).
- **The compiled `view.length` read serves the slots directly.** A new
  `typed_array_length` helper (the four-file mirror: a pure
  `(ctx, object)` probe returning the slots length for an IntegerIndexed
  receiver or the canonical-NaN sentinel) fronts the compiled
  `GetMemberName`-with-`length` site before the member-cell probe — a
  hit skips the `get_member_name` FFI round-trip. The probe is exact for
  the same receivers the interpreter's fast path serves (the
  own-`length` shadow is handled on the interpreter side); every other
  receiver falls through to the member-cell probe / helper unchanged
  (`{ length: 5 }` reads, string `.length`, etc.). The helper is
  whitelisted as leaf-eligibility-neutral (pure, `emit_raw_call`).
  Measured: the JIT length row **19.5ms → ~11.3ms (~42%)** (3 runs,
  stable); the interpreter row is unchanged (its own fast path already
  served the slots).

With these, the table's earlier gaps collapse: the JIT length row is
~11x vs node (was ~56x per the older table) and the JIT write row ~30x
(was ~90x); both rows are the closest to node in the suite alongside
buildString.

### Vector-call leaf-inline and the apply/call leaf fast path (measured 2026-09-01)

The last call-step gap from `docs/jit-report.md` §7 item 7 — "a vector
call to a certified LEAF still runs the general `call_inner`" — is closed,
and `Function.prototype.apply`/`call` gained the same leaf fast path. New
`--jit-bench` rows: `vector leaf call` (9-arg leaf, 200K) and
`apply leaf call`. (The `vector leaf call` row now uses 17 args — see the
fast-argument cap note below.)

- **The interpreter's vector-form call (`Step::Call` — the ≥9-arg or
  spread form) now leaf-inlines.** `do_call` rebuilt the fast-form layout
  on the value stack and routed through `do_call_fast`, the same shared
  core the JIT's `call_vector` helper already used (Cut 50): a
  certified-leaf callee runs inline on the caller's Vm — no
  execution-context push, no fresh-Vm round trip. The JIT side was
  already there; the interpreter's `do_call` was the remaining
  general-`call_inner` gap. Direct eval, the callable check, and the
  general path all come from the shared core unchanged (the old handler's
  special cases deleted). Measured on the new row: **interp 53.2ms →
  21.3ms (2.5x)**, within 1.25x of the JIT's ~17ms.
- **`f.apply(this, arr)` / `f.call(this, …)` to a certified leaf run the
  leaf machinery.** The builtins previously built the arg list and went
  through `crate::function::call` → `ordinary_call` (the general path —
  fresh Vm + execution-context push). New `try_leaf_call` runs a
  certified-leaf callee through `do_call_fast` on a POOLED Vm —
  register-op (or JIT machine-code) body execution with no context push.
  It mirrors `fast_call_core`'s leaf gate: a single realm, an EcmaScript
  function, and a `leaf_lookup` hit — which only ever caches
  `leaf_inline` bodies (`set_compiled` excludes class constructors and
  class-field initializers), so `C.apply(null, [])` still throws the
  "must be called with new" TypeError through the general path. Design
  notes: it deliberately does NOT route through the running Vm (the
  caller's `&mut Vm` and an args slice into its stack are live across the
  native-handler call — mutating it through a raw pointer would be UB),
  and the pooled Vm is registered as an `ACTIVE_RUNS` entry for the whole
  leaf window (`with_leaf_run`) so a budget collection inside the body
  traces its stack exactly like `run_inner` (verified under
  `--gc-stress`). Measured (A/B, 3 runs each, alternating order): **interp
  235ms → 208ms (~11%), jit 223ms → 204ms (~8.5%)** on the new row;
  apply-to-leaf is also ~100ms faster than apply-to-a-non-leaf (315ms),
  i.e. the callee dispatch is the saved part.

**Remaining apply floor (follow up later).** The apply row is still ~1µs
per call (208ms/200K) with the leaf callee itself only ~85ns — the floor
is the builtin round-trip, not the callee: reaching `apply` through the
general call machinery, `create_list_from_array_like`'s per-call `Vec`
build (the dense fast path exists but still copies the elements), and the
pooled-Vm take/return resets. Cutting it needs call-site recognition of
the `.apply` pattern — inlining `f.apply(a, arr)` as a spread/vector call
in the compiler (the V8 approach) — a substantially larger slice than the
leaf-inline here. **CLOSED 2026-09-01** — see the compiled `CallApply` step
section below.

### Call-site `.apply`/`.call` recognition: the compiled `CallApply` step (measured 2026-09-01)

The apply floor is closed by recognizing the `.apply`/`.call` member-call
pattern in the compiler (`Compiler::try_compile_apply_call`): `f.apply(x,
arr)` / `f.call(x, ...)` in a certified body compiles to the normal member
read (a shadowed `apply`/`call` resolves onto the stack and is called
normally) plus the new `Step::CallApply`, whose handler compares the
resolved function against the realm's cached intrinsic
(`Intrinsics::apply_builtin`/`call_builtin`) and, on a match, calls `f`
directly on the caller's Vm — the `this` argument, the
`CreateListFromArrayLike` element reads (dense-array fast path included),
and the leaf-inline `do_call_fast` run with no builtin dispatch, no
`leaf_lookup` HashMap, and no pooled-Vm take/return. Any other resolved
function falls back to the general call of that function exactly as
`CallFast` would, so shadowing, the is-callable-before-argArray order
(the intrinsic's step-1 TypeError runs before any getter), array-like
(non-Array) arg lists, getter elements, holes, and the extra-arg ignore
stay spec-exact. The JIT lowers the step through the `call_apply` slow-path
helper (the four-file mirror), so compiled bodies with `.apply`/`.call`
keep compiling.

Measured on the `apply leaf call` row (200K, release, 3-run medians):
**interp 208ms → ~101ms (~2.05x), jit 204ms → ~89ms (~2.3x)**; the
remaining ~90-100ms is the per-call member read, the `CreateListFromArrayLike`
length/element reads, and the leaf-inline frame setup (the leaf itself is
~85ns). The `call_apply` helper also means a compiled body containing a
`.apply` call no longer bails out of the JIT. (The arg-list build still
allocates a per-call `Vec` for the general path — the dense fast path
avoids the per-element key/Get machinery, not the allocation; inlining
the dense element pushes onto the stack is a listed follow-up.)

**Dense-array argument list** (2026-09-01): the listed follow-up landed.
`do_call_apply`'s `Apply` arm now recognizes a dense Array argArray and
pushes its elements straight onto the value stack — no per-call `Vec`
allocation, no `length` property path / `ToLength` round trip, no
`[[Get]]`-loop element reads. The gate is exact: a hole, a length past
the buffer end, or a non-Array falls back to
`create_list_from_array_like` (whose own fast paths and the spec `[[Get]]`
loop keep those shapes unchanged), and the buffer borrow never spans the
call (a re-entrant callee may mutate the argument array). Measured on the
`apply leaf call` row (200K, release, A/B medians): **interp ~101ms →
~57ms, jit ~98ms → ~50ms (~1.8x / ~1.9x)** — the JIT's `call_apply` helper
inherits it automatically. Spec-exact: the new
`apply_dense_array_fast_path_preserves_spec_semantics` test (holes,
partially-filled arrays, re-entrant mutation, 10k-element arrays,
array-likes, getters) plus the Function/prototype/apply+call fixture
clusters clean under `--gc-stress --jitless`; full sweep (JIT default and
`--jitless`) at 0 fail / 0 crash / 0 hang.

**Fast-argument cap raised to 16** (2026-09-01): `FAST_CALL_MAX_ARGS` 8 →
16 — plain calls with 9-16 arguments now take the fast `CallFast`/`TailCallFast`
form (one step, the leaf-inline probe) instead of the vector form, whose
JIT path ran 11 helper calls per iteration (`ArgsBase` + 9×`ArgsPush` +
`Call`). The two `[Value; FAST_CALL_MAX_ARGS]` stack buffers
(`do_call_apply`, `run_inline_leaf`) grow with the cap; a 17+-arg call or
a spread still takes the vector form. Measured on the `vector leaf call`
row's 9-arg shape (200K, release): **interp ~22.5ms → ~14ms, jit ~17.9ms
→ ~2.4ms (7.4x)** — the 9-arg leaf now runs fully in machine code. The
row was bumped to 17 args so the vector form stays benchmarked (interp
~30ms, jit ~21ms). The vector-form JIT tests that used 10-arg calls
switched to 17 to keep their coverage. Spec-exact: the new
`wide_fast_form_calls_stay_spec_exact` test plus the full sweep (JIT
default and `--jitless`) at 0 fail / 0 crash / 0 hang.

**Fast-argument cap raised to 32** (2026-09-02): `FAST_CALL_MAX_ARGS` 16 →
32 — plain calls with 17-32 arguments now take the fast form too. The
remaining `vector leaf call` row's 17-arg shape (200K, release, A/B):
**interp ~28.8ms → ~17.0ms, jit ~21.4ms → ~2.8ms (7.6x)** — the 17-arg
leaf now runs fully in machine code (the vector form's JIT path ran 18
helper calls per iteration: `ArgsBase` + 16×`ArgsPush` + `Call`). The
row was bumped to 33 args so the vector form stays benchmarked (interp
~44-48ms, jit ~31-32ms); the vector-form JIT tests that used 17-arg
shapes switched to 33 to keep their coverage. Spec-exact: the
`wide_fast_form_calls_stay_spec_exact` test now covers the 33-arg
vector boundary, plus the full sweep (JIT default and `--jitless`) at 0
fail / 0 crash / 0 hang.

**Prototype-chain member-read cache** (2026-09-01): the remaining
member-read cost of the apply path — `f.apply`'s `apply` lives on
`Function.prototype`, so the own-property member cells never serve it and
every read paid the full prototype walk. A new agent-level
`member_chain_cells` cache stores the resolved chain value for
`(receiver, name)`, re-validated by the receiver's generation and each
walked link's (id, generation) (an own property on the receiver, a link's
mutation, or a proto replacement bumps a generation; links below the
found one cannot shadow its own data property, and accessors are never
cached). This serves every method read (`f.apply`, `arr.push`, `o.m()`),
not just the apply path. Measured on the `apply leaf call` row (200K,
A/B): **interp ~57ms → ~20ms, jit ~52ms → ~18ms (~2.8x / ~2.9x)** — the
interpreter's member read and the JIT's `GetMemberName` slow helper both
hit the cache. Spec-exact: the new
`chain_member_cache_stays_spec_exact_under_invalidation` test (own-prop
shadowing, link redefinition, accessors run per read, proto replacement,
`Function.prototype.apply` patching) plus the full sweep (JIT default
and `--jitless`) at 0 fail / 0 crash / 0 hang.

**The JIT inline prototype-chain probe was implemented and reverted
(measured regression, 2026-09-01).** The natural follow-up to the cache
above — the JIT's `emit_member_cell_read` still called the
`GetMemberName` helper for chain reads (`f.apply`), so a compiled probe
was added to its slow path: a Function receiver hops through
`crux::Function.object` to the receiver JsObject, the cell's (id, name,
receiver generation, link_count) is validated, and the cached links'
(id, generation) are walked against the LIVE `prototype` fields — a hit
serves the cached value with no helper call, a miss falls through to
`GetMemberName` exactly. Soundness mirrors the interpreter (receiver
generation covers own-property changes; each link's generation covers
its mutation or a proto replacement; accessors are never cached). A/B
vs the committed cache (HEAD `2b787ff`, same machine, interleaved 20M
runs): the probe is **~5-10% SLOWER than the helper** on pure chain
reads — `o.m` on a prototype 424-440ms → 469-486ms; `f.apply`
620-638ms → 681-716ms — and the call-dominated `apply leaf call` row
(200K) did not move (jit ~18.3ms → ~19.0ms, within noise but trending
worse). The helper's ~13-16 L1 loads are better scheduled by LLVM than
the raw Cranelift probe, and the call overhead is tiny, so an inline
probe that duplicates the validation cannot win. Reverted; the
interp-side cache served inside the helper remains the win. Traps for a
future attempt: (1) `JsObject` and `MemberChainCell` are repr(Rust),
NOT `#[repr(C)]` despite the doc comments — `JsObject.id` sits at
offset 24 and `crux::Function.object` at 96 — `offset_of!`/`size_of!`
kept the probe consistent, but a stable ABI needs `repr(C)` first; (2)
to win, a probe must CUT the per-read validation (the link-generation
walk is the irreducible ~4 loads, the cell fields ~6-9 more), not
duplicate the helper.

### The interpreter's `LoadIdent` global fast path, mirroring the JIT's Cut 36 probe (measured 2026-09-01)

The `--jit-bench` `global read` row (a 1M `s += g` loop inside a function)
measured ~157ms interpreted — ~10x the same loop reading a local — while
the JIT ran it in ~6.5ms. The gap: a function body reads a script-global
through `Step::LoadIdent` (function bodies carry no `script_globals`, so
`binding()` classifies globals as env-path), and the interpreter's
`LoadIdent` handler walked the env chain + built a `PropertyKey` + ran the
full `Get` per read (~133ns/read).

- **The interpreter now mirrors the JIT's Cut 36 probe.** `Vm::run_inner`
  computes `clean_chain` at entry — the running context's env IS the
  global env record with no outer (the same gate `JitCallContext` bakes
  in) — and the `LoadIdent` handler, gated on `body.env_constant &&
  clean_chain` (an env-constant body adds no envs mid-run, so the entry
  flag stays valid), serves the warmed global-value cell directly when
  the captured name + the live global's id/generation match, else falls
  back to the full resolve. The miss path warms the cell with the JIT's
  exact `load_ident` gate (a Global-env binding with no top-level
  `let`/`const`/`class`), so a hot loop's second read is a native load.
- **The warm gate now records only own DATA properties** — a real bug fix
  that also corrects the JIT's probe: `warm_global_cell` and
  `load_global_value`'s fallback previously recorded the value cell even
  when the binding was an accessor or inherited property, so the probe
  served a stale first-read value forever (an accessor's getter ran once;
  an inherited `Object.prototype` member could change without the global's
  generation bumping). Both paths now skip recording when
  `resolve_global_cell` finds no own-data slot.

Measured: **`global read` interp 157.6ms → ~49ms (~3.2x)** — parity with
reading a local in the same loop shape — with the JIT row unchanged
(~6.4ms); no regressions on the other rows. The remaining ~49ms was the
row's general step-loop floor (the `i < n` limit is a slot, not a
literal, so the fused head did not apply); the fast-binding-limit fusion
(`RelLimit`, committed 2026-09-01) later closed that part — the row
measures ~21.6ms now — and the post-inc store slice below trims the
store shapes.

### Register-encoded post-increment computed stores (measured 2026-09-01)

The `indexed store` per-op floor (`a[l++] = i`, ~48ns/op) decomposed to
~7 step dispatches per iteration plus the store machinery; the body would
not lower to a register body because `l++` (a postfix `UpdateLocal`) had
no register form and the loop counter could not serve as a store value.
The register model now covers the shape:

- **`RegOperand::PostInc { slot, tdz, op }`** — a post-increment member
  key: loading the operand reads the slot (TDZ-checked), writes the
  update back through the shared `update_value` coercion (a non-Number
  `l` goes through ToNumeric), and yields the OLD value — the postfix
  `UpdateLocal` semantics, with the write-back landing before the
  store's nullish check (JS evaluation order). The lowering emits it
  from `Step::UpdateLocal` (`prefix: false`), and only the computed
  member store consumes it — a statement-position update
  (`a[l] = i; l++;`), a prefix form, a read key, or a binary operand
  keeps the body on the step path (where the ordering is preserved).
- **The loop counter as a store key/value** — a `RegOperand::Counter`
  key or value resolves from the dedicated `Vm::loop_counter` field at
  load time (mirroring `LeafOp::LoadCounter` and the JIT's
  `counter_bits`). Since Cut 35 slice 21 the register path never pushes
  the counter onto the value stack at run entry, so the operand must
  not pop one — the slice-16-style pop read a stale stack slot /
  `undefined` in the interpreter (the JIT was already reading the
  field). The single-read guard is unchanged.
- **The JIT mirrors the operand** — `leaf_operand`'s `PostInc` arm emits
  the `emit_update` fast/slow shape (inline f64 add + `UpdateValueSlow`
  fallback) and writes the slot back, so a compiled body containing the
  shape keeps compiling (no bail regression under the default JIT).

The body `a[l++] = i` compiles to `RunRegBody { LoadReg a;
StoreMemberComputed { key: PostInc(l), value: Counter } }` — one dispatch
instead of seven steps. Measured (release `--bench`, medians of 3):
**`indexed store` 47.2ms → ~42.7ms (~10%)**, the 5M-iteration post-inc
probe 246ms → ~222ms (~5.6ns/op saved). The `buildString shape` row (a
`l++` store plus an `if (l === 10000)` guard) stays on the step path —
the register lowering is whole-body and rejects the guard's branch; a
per-statement register-run segmentation is a listed follow-up. Spec-exact:
`register_post_inc_member_store_stays_spec_exact` plus the full sweep
(JIT default) at 0 fail / 0 crash / 0 hang.

### Per-statement register-run segmentation (measured 2026-09-01)

The post-inc lowering was whole-body: a loop body containing any
control-flow statement (an `if`, a break) rejected the entire body back
onto the step path, leaving the `buildString shape` row (the `l++` store
plus an `if (l === 10000)` guard) unlowered. The compiler now segments
the body's compiled steps into maximal straight-line runs
(`lower_leaf_ops_segmented`): each run lowers via the shared per-step
`lower_step` (the whole-body `lower_leaf_ops` became a wrapper over it)
and is replaced with its own `Step::RunRegBody`, with the branch and
completion steps between runs staying on the step path
(`apply_register_runs` re-bases the labels and fixups recorded from the
body start, and a label landing strictly inside a run keeps the whole
body unsegmented).

Three soundness constraints the segmentation must honor:

- **The list wrappers stay paired on the step path** — a run must never
  absorb a `ListBegin`/`ListEnd`: an absorbed `ListBegin` with a
  step-path `ListEnd` pops the ENCLOSING block's completion entry
  (nested loops in blocks restored a stale value into the script
  completion). The run commits at the statement boundary before a
  wrapper and the wrapper executes on the step path.
- **A run must not start at a `SetCompletion`** — absorbing a
  statement's `SetCompletion` without the statement's value-producing
  steps leaves the statement's result on the stack: a one-slot
  per-iteration drift in the compiled path, which pre-allocates its
  working area from `max_stack_usage` (the JIT crashed on
  `o[i] = i; b['x'] = i` with a Vec-corruption panic).
- **The register ops' literal values are traced** — a member store's
  `Const` key string lives in the `RunRegBody` ops; `Step::trace`
  missed them, so the per-allocation collector swept the box mid-run
  and the key read back as garbage under `--gc-stress`
  (`Step::RunRegBody` now walks the ops via `trace_leaf_op_heaps`).

Measured (release `--bench`, medians of 3, A/B against the post-inc
tree): **`buildString shape` ~193.8ms → ~181.9ms (~6%)** — the `l++`
store runs as a two-op register body while the guard `if` (condition,
branch, nested block) runs on the step path. The `indexed store` row is
unchanged (~45ms under load). Spec-exact: new tests
`segmented_loop_body_keeps_list_wrappers_balanced`,
`register_counter_member_operands_stays_spec_exact`, and
`register_run_ops_are_rooted_under_gc_stress` plus the JIT
script-completion table's segmented shapes; the full sweep (JIT default
and `--jitless`) at 0 fail / 0 crash / 0 hang, and the `statements/for*`
cluster clean under `--gc-stress --jitless`.

### Node comparison on the `--jit-bench` suite, JIT and JITless (measured 2026-09-02)

The 2026-08-21 Node comparison predates the bytecode-VM migration and the
Cranelift JIT; re-measured against node v24.12.0 on the same machine and
session, in both JIT (default) and interpreter-only (`--jitless`, V8's
Ignition bytecode interpreter) modes, over the full `--jit-bench` suite
(12 rows). Harness: `scratch/jit_bench/node_bench.js`, running the exact
sources. Slag columns are the best of 3 `--jit-bench` process runs (each
mode = one warmup eval + one timed eval in a fresh context); Node columns
are the best (steady-state) round of 3 warmup calls + 5 timed calls. All
four modes agree on every row's completion value (spot-checked
`buildString full` → 2162678 in all four).

| Benchmark | slag interp | slag jit | node jitless | node jit | interp gap | jit gap |
|---|---|---|---|---|---|---|
| arithmetic | 26.2 | 3.3 | 10.2 | 0.58 | 2.6x | 5.7x |
| property read | 54.5 | 6.9 | 12.4 | 0.32 | 4.4x | 21.8x |
| string concat | 10.6 | 2.7 | 1.5 | 0.53 | 7.1x | 5.0x |
| function calls | 6.3 | 1.9 | 1.9 | 0.06 | 3.3x | 32x |
| global read | 23.2 | 3.8 | 7.8 | 0.32 | 3.0x | 12.1x |
| compound assign | 19.7 | 2.5 | 1.6 | 0.06 | 12.6x | 41.5x |
| buildString shape | 180.1 | 54.2 | 53.1 | 8.2 | 3.4x | 6.6x |
| buildString full | 86.9 | 32.3 | 26.2 | 10.2 | 3.3x | 3.2x |
| typed-array write | 75.0 | 30.2 | 13.6 | 0.29 | 5.5x | 103x |
| typed-array length | 59.3 | 11.4 | 16.9 | 0.47 | 3.5x | 24.3x |
| vector leaf call | 45.0 | 29.7 | 9.5 | 0.12 | 4.8x | 258x |
| apply leaf call | 20.4 | 18.3 | 6.1 | 2.16 | 3.4x | 8.5x |

(Gaps are slag ÷ node — how many times faster Node is. Times are ms.)

- **Interpreter vs interpreter is the closest picture.** Slag's
  interpreter trails V8's `--jitless` Ignition by a median ~3.4x (range
  2.6x–12.6x) — same order of magnitude for a young step-loop VM. The
  outliers are `compound assign` (12.6x) and `string concat` (7.1x), the
  shapes where V8's interpreter keeps a fast-path edge Slag's step loop
  lacks.
- **JIT vs JIT is the big gap.** Slag's Cranelift JIT trails V8's
  TurboFan/Sparkplug by a median ~17x (3.2x–258x). The worst rows are the
  whole-loop-specialization shapes: `vector leaf call` (258x — V8 inlines
  the 33-arg leaf into the loop and eliminates the dead args; Slag runs
  its per-iteration leaf-inline probe + register protocol), `typed-array
  write` (103x — no bounds-check-elided store loop), `compound assign`
  (41.5x) and `function calls` (32x). The narrowest are the
  inlined-arithmetic shapes — `buildString full` (3.2x), `string concat`
  (5.0x), `arithmetic` (5.7x) — where Slag's JIT keeps the hot
  counter/accumulator register-resident, closest to TurboFan's output.
- **Slag's JIT roughly matches Node's interpreter-only mode** — parity or
  better on 6 of 12 rows (`arithmetic` 3.1x faster, `global read` 2.0x,
  `property read` 1.8x, `typed-array length` 1.5x, and parity on
  `function calls`/`buildString shape`), trailing 1.2x–3.1x on the rest.
  This is the realistic "JIT vs a modern interpreter" read.
- **JIT efficiency over each engine's own interpreter**: Slag 1.1x–8.0x
  (median ~3.7x); Node 2.6x–82x (median ~25x) — V8's tier-up buys ~7x
  more, because its interpreter is already fast and TurboFan specializes
  aggressively (hidden classes, inline caches, callee inlining).

Methodology note: Slag's jit column is slightly pessimistic — the timed
eval re-parses the snippet and pays a fresh Cranelift compile (~1ms per
tiny body, see `bench_once`) — while Node is at full tier-up after 3
warmup calls, so the true JIT gaps are a bit smaller than shown. These
are micro-benchmarks of the JIT's supported subset, not a workload
comparison.

### L1a — the warm-store fast path (measured 2026-09-03)

The performance plan's L1a (a store-side cell mirroring the read cells)
landed: `put_value`'s member write to an Ordinary object/function with
receiver == base and a string key now probes a generation-validated cell
— `(object id, name, generation, slot)` on `Agent::member_write_cells` —
recording "at this generation, `name` is an own writable data property
at property-vector `slot`". On a hit the write calls
`JsObject::write_data_property_slot` (a direct vector-slot write +
inline-field mirror + generation bump) and refreshes the read-side value
cell, skipping `put_value`'s namespace/receiver probes, the primitive
boxing, `find_ecma_accessor`'s own-property lookup, and
`set_with_receiver_key`'s second descriptor lookup. Sound because an own
writable data property shadows the whole chain (spec 7.3.3 step 3
consults the chain only when the own property is absent) — no setter
tracking needed — and the slice-11 discipline (every own-property
mutation bumps the generation) invalidates on redefinition/delete/
accessor-conversion. The cell is filled after a cold full-[[Set]] write
that leaves an own writable data property. Interpreter-only: the JIT's
compiled fast member-store is a separate path (its `call_slow`
fallbacks inherit the win through `put_value`).

Probe (the plan's next-experiment gate): the `compound assign` row is
`o.x += 1; s += o.x` per iteration — the write term of the row's ~186ns/
iter (2026-09-03 decomposition, ~146ns of it the write). Interleaved
A/B of `--jit-bench` on this worktree vs its parent (ba9a69b), same
machine: compound assign interp drops 17.5ms (parent, 4 runs 17.1-17.9)
-> 6.16ms (4 runs 6.08-6.24) — ~2.8x, ~175ns -> ~62ns per iteration. The
JIT column is unchanged (~1.45-1.5ms); the other `--jit-bench` and
`--bench` rows are unchanged within the machine swing.

Two slices beyond the cell itself close the write path: (1) the store
cell is now probed directly from `assign_member`'s Assign/compound/logical
branches BEFORE `fast_fresh_store` and `put_value` — the hot existing-
property write skips the fresh-store map check, the `member_reference`
build, and the `put_value` call layer (put_value keeps its own probe for
its other callers: updates, destructuring, eval); (2) `member_reference`/
`super_reference` no longer round-trip a `Name` atom through
`crux::lookup` + re-intern — `PropertyKey::String(id)` is the canonical
form, so the clone-and-rehash was pure waste on every member write. The
second read per iteration also hits the value cell the store refreshed
(before, the write's generation bump forced it to re-resolve).

Gates: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` green; the three release sweeps are identical
to the parent (ba9a69b) sweep on every count — language 23721/23724
(3 skip), built-ins 23657/23812 (155 skip), annexB 1086/1086, all with
zero fail/crash/hang on both binaries; the edge probe
(`scratch/l1a_store_probe.js`: accessor conversion, delete+recreate,
chain setters present and added later, non-writable + strict,
function objects, multi-prop thrash, computed keys, super writes,
proxies, own accessors) passes under the JIT, `--no-jit`, and
`--gc-stress`.

Next experiment: L1c (shapes with inline-property offsets), the plan's
primary architectural investment. The L1b fuse was skipped ahead of it:
L1b's probe showed the compound row's residual ~20ns of non-write
overhead (of ~62ns/iter) is only partly the read-modify-write split, and
L1c's timing note — the cell model only accretes more property paths the
longer it stays — argues for the shape work first. The first L1c write
slice landed below.

### L1c-1 — pinned inline-field mirror in the store cell (measured 2026-09-03)

The performance plan's first L1c write-path slice: the L1a store cell now
records the property's inline-field mirror — the (map id, in-object
field offset) assigned by the object's map (`JsObject::map_store_field`)
— alongside the property-vector slot. A warm write mirrors the value
into `in_fields` at the pinned offset directly (one map-id compare)
instead of re-scanning the map's descriptors via `map_set` on every
store. Sound under the same generation gate that already pins the slot:
every structural change (define, delete, accessor conversion, map
transition — all of which run through `define_property_key`'s entry bump
or `delete_key`) bumps the object generation, and a map id pins its
descriptor layout (maps are immutable after creation). The pinned write
re-checks the map id as a backstop for a missed bump; a mismatched or
dropped map (dictionary mode) and vector-only properties (non-w/e/c
defines, the >4-property spill) fall back to the `map_set` scan. The
write cell is interpreter-only, so the cell layout is not JIT-visible.

Probe (the plan's next-experiment gate): the mirror-scan share of a warm
write. A/B of the `compound assign` row (a 1-descriptor object, scan
depth 1) between fresh full builds of this worktree and its parent
(20be822), interleaved at high priority on a CPU-saturated machine
(llama-server pegged all 16 cores, so absolute deltas carry extra noise;
identical-source control builds moved ±12% on the untouched `arithmetic`
row from code-layout luck alone): compound assign interp is neutral —
6.10ms (parent, 5 runs 6.08-6.27) vs 6.08ms (5 runs 6.01-6.09). The
depth-sensitive probe (`scratch/l1c_ab.js`, a certified-leaf row writing
the LAST of a 4-descriptor map, 4M iters) moved 255.5ms (parent median)
-> 247.5ms — ~3%, ~2ns/iter at scan depth 4 — with the depth-1 control
moving ~1.6% in the same direction, so the net scan saving is ~1-2ns at
depth 4 and below resolution at depth 1. The row's residual cost is not
the mirror scan.

Gates: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` green (incl. four new crux tests:
`map_store_field_*`, `pinned_field_write_*`); the three release sweeps
are identical to the parent on every count — language 23721/23724
(3 skip), built-ins 23657/23812 (155 skip), annexB 1086/1086, all with
zero fail/crash/hang; the edge probe (`scratch/l1c_store_probe.js`:
4-descriptor warm writes, sibling/overflow props, accessor conversion
after warm, delete+recreate on a full map, vector-only non-enumerable
defines beside a live map, shared-shape instances, function objects,
super receivers, proxies, >16-key thrash) passes under the JIT,
`--no-jit`, and `--gc-stress`; the L1a probe still passes.

The slice's value is the pinned-offset mechanism: the store cell now
carries the map/field pair the deeper L1c write phases need (shape-check
field writes that drop the per-write vector/descriptor machinery).

Decomposition probe (the recorded next step, 2026-09-03): where the
~63ns/iter `compound assign` row actually spends, isolated in the
certified-leaf regime (`scratch/l1c_write_decomp.js`, 2M iters, medians of
5 runs): the loop+var floor is ~10ns/iter (f6, `s = i`); the warm member
WRITE is ~22ns (f5 `o.x = i` minus the floor); a serial dependent `+=`
chain costs ~12-14ns per chain (f0 `s += i` = 22 vs f6 = 10), and the row
carries TWO such chains (`o.x`'s RMW and the `s` accumulator); the warm
member read is ~3ns. The compound step is NOT a tax: `o.x += 1` (f2,
~49ns) equals the plain `o.x = o.x + 1` (f9, ~47ns) — both pay the read +
serial chain on `o.x`. So the row is roughly floor 10 + write 22 + RMW
chain ~14 + s chain ~14 + reads ~4 ≈ 64. Conclusion: the L1b fuse was
right to defer — fusing the compound buys nothing the plain RMW already
pays (the earlier "~20ns of non-write overhead" premise was the chain
latency, which fusion cannot remove).

Attribution probe (what the structural L1c write could actually reclaim,
2026-09-03): a throwaway variant removing the property-vector write
entirely from `write_data_property_slot` (the RefCell borrow, the
backstop re-checks, and the vector slot store — the field mirror,
generation bump, and both cell records kept) drops f5 from ~31.5 to
~26.5ns/iter (write ~22 -> ~17ns) and leaves the row (f3) unchanged at
~62ns. So the vector write is only ~5ns of the ~22ns write; the
field-authoritative rewrite (making the map field the storage authority
for mapped keys, shrinking SmallProps to overflow) removes ~5ns of the
~63ns row (~8%) at the cost of making every descriptor/enumeration/
structural consumer field-aware. That trade is NOT justified: the
remaining ~17ns is the field mirror + generation bump + write-cell
re-record + value-cell front + interpreter dispatch — the generation/
record discipline the JIT's compiled probes share. Cutting it requires
the deep L1c+JIT step (shape-based reads in BOTH engines so the
value-cell records die), and even then the row stays ~45-50ns because of
the two ~14ns dependent chains + 10ns floor. The row is now executor-
bound, not property-bound: reads + vector write are ~8ns of the 63, and
further interpreter property slices (L1c write or read) have low measured
ceiling on it. The measured levers left are the register executor's
dependent-add latency (shared with the `arithmetic` and `property read`
rows) and the L1c-JIT record-discipline redesign.

### Register-run coverage: the loop counter as a binary/store operand (measured 2026-09-03)

Refining the decomposition above: the ~12ns "dependent-chain" cost on
pure-var chains was NOT register-executor latency — it was a step-path
fallback. Certified acc-path loop bodies stayed on the step path whenever
the loop counter (the `PushAcc` operand, the dedicated `loop_counter`
field) appeared as a binary RIGHT operand (`for (var i..) { s += i }`) or
a plain member-store VALUE (`o.x = i`): the register lowering rejected
`RegOperand::Counter` in both positions, so the whole body dispatched per
step. The lowering now admits it: the binary arm loads the counter first
and combines a spilled / late-readable frame-slot left
(`LoadCounter` + `BinAccPop`/`BinLeftReg` — safe for `tdz=false` slots,
where nothing in the straight-line run writes the slot between the load
and the combine), and `StoreMemberName` resolves the counter from the
dedicated field at execution time (after the object load and nullish
check). Both reuse existing leaf ops, so the executor and the JIT need no
new arms — newly-register-run bodies are `RunRegBody`s of ops both
engines already emit.

Measurement (certified-leaf probes `scratch/l1c_write_decomp.js`,
high-priority interleaved runs, new vs the parent fresh build a962bb6):
f0 `s += i` ~21 -> ~13ns/iter; f5 `o.x = i` ~31 -> ~22.5; f4
`o.x = i; s += o.x` ~44.5 -> ~34. Controls flat: f6 `s = i` 9.5, f1
`s += o.x` 16.5 (the earlier 23.5 reading was a code-layout artifact of an
incremental build), f2/f8/f9 (member compound, step path by design)
47-52. The 12 `--jit-bench` rows are unchanged (fresh-build A/B:
arithmetic 11.99 vs 11.97, property read 23.30 vs 22.94, compound assign
6.01 vs 6.04) — the suite has no counter-fed var-accumulator or
member-store row; the slice widens register-run coverage to the common
`for`-loop shapes that feed a var or store the counter onto a member.

Gates: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` green (incl. a new eval test asserting both
shapes lower to `RunRegBody`); the edge probes pass under the JIT,
`--no-jit`, and `--gc-stress`; the three release sweeps are identical to
the parent — language 23721/23724 (3 skip), built-ins 23657/23812
(155 skip), annexB 1086/1086, all with zero fail/crash/hang.

Next: the compound row's residual cost is the member-compound step path
(f2/f3 unchanged here) and the member-read-fed chains.

### Member-store register fusion: `o.x op= v` and `o.x = <computed>` in runs (measured 2026-09-03)

The member-RMW bodies land on the register executor. The row's `o.x +=
1` body dispatched 9 steps/iter (LoadLocal o; Dup; GetMemberName; Push;
AssignMemberName compound; SetCompletion; ListEnd; head) and a plain
computed-value store (`o.x = o.x + 1`) stayed step-path purely because the
value was live in the accumulator. The lowering now handles all three
blockers: `Step::Dup` (a loadable shadow operand duplicates by re-read);
the compound member assign decomposes into the plain binary on the cached
old value plus a plain store — sound because `apply_compound(op, l, r)`
IS `apply_binary(compound_binary(op), l, r)`, so `o.x op= v` ≡ read +
binary + plain store (a setter/chain receiver runs exactly once, on the
write); and a computed (`Acc`) member-store value with a frame-slot object
stores via the new `StoreMemberNameLocal` leaf op, which reads the object
from its slot at store time so the value never round-trips the
accumulator. The RHS must be a pure operand (Reg/Const/Ctx/PerIter, plus
the loop Counter via a PushAcc+LoadCounter+BinAccPop spill) and the object
a `tdz=false` frame slot (the late-read contract). The logical assigns
(`&&=`/`||=`/`??=`) stay on the step path (they short-circuit). The JIT
emits `StoreMemberNameLocal` with the step path's inline validated store
(`obj_ok` gate + member-value-cell probe + `set_member_slot`), so the
compiled compound column does NOT regress to the slow helper.

Measurement (certified-leaf probes `scratch/l1c_write_decomp.js`, 2M
iters, 5-run medians, high-priority interleaved; the machine was noisy so
the floor drifted 9.5-11): f2 `o.x += 1` ~48.5 -> ~29.5ns/iter; f8
`o.x += i` ~49 -> ~33; f9 `o.x = o.x + 1` ~47 -> ~26.5; the full row
(f3) ~62 -> ~40.5. The `--jit-bench` compound-assign interp row drops
6.0 -> ~3.9ms/100k (~35%, 62 -> ~39ns/iter) with the JIT column flat
(1.44ms vs 1.43) and the other rows unchanged within the machine swing.

Gates: `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` green (incl. two new eval tests asserting the
counter-fed and member-RMW bodies lower to `RunRegBody`); the edge probe
(`scratch/l1c_compound_probe.js`: every binary compound op, string +=,
own getter/setter accessors, chain accessors with and without an own data
shadow, non-writable, delete+recreate, logical assigns, counter RHS,
expression-position compounds, nullish receivers, undefined +=
NaN, descriptor/enumeration integrity, mid-loop break) passes under the
JIT, `--no-jit`, and `--gc-stress`, as do the L1a/L1c store probes; the
three release sweeps are identical to the parent — language 23721/23724
(3 skip), built-ins 23657/23812 (155 skip), annexB 1086/1086, all with
zero fail/crash/hang.

The same mechanism then covers statement-position member UPDATES
(`o.x++` / `o.x--`): the lowering decomposes `Step::UpdateMemberName` into
the read + a new `UpdateAcc` leaf op (ToNumeric ±1 — NOT the binary `+`,
which would concatenate a numeric string — with the number case inlined
and `update_value` as the fallback) + `StoreMemberNameLocal`. The update's
expression result (old/new) is discarded at the statement boundary, so
prefix and postfix lower identically; an expression-position update
(`s += o.x++`) leaves its value unconsumed and stays on the step path
(asserted by the new eval test). The JIT emits `UpdateAcc` with the
`emit_update` fast/slow shape (inline f64 ±1, `UpdateValueSlow`
fallback). Measured: `o.x++` ~48 -> ~27ns/iter and `o.x--` ~27.5 in
certified loops; the compound row and all jit-bench rows unchanged.
Gates: clippy clean, workspace tests green, the update probe
(`scratch/l1c_update_probe.js`: numeric/numeric-string/NaN/Infinity
updates, postfix/prefix values, own accessors, chain accessors,
non-writable, delete+recreate, nullish, mid-loop break) passes under the
JIT, `--no-jit`, and `--gc-stress`; the three release sweeps are
identical to the parent.

### Computed-member RMW on the register executor (measured 2026-09-03)

The computed forms of the member RMW — `o[k] += v` (statement position),
`o[k]++`/`o[k]--`, and the explicit `o[k] = o[k] + 1` — never reached the
warm member cells: they ran the full Get/Set/Computed machinery per
iteration (each iteration converting the key and re-resolving the
property), measuring ~158ns/iter vs the named `o.x += 1` at ~28ns (the
probe `scratch/computed_probe2.js`). The statement-position compound and
update now lower to ONE fused register op per iteration
(`CompoundMemberComputedLocal`, `UpdateMemberComputedLocal`; the
`Dup2`/`GetMemberComputedKeep` read is deferred into it — sound because
the RHS must be a pure operand, which emits no op between), and the
plain-with-computed-value store (`o[k] = o[k] + 1`) lowers to the
computed read + `StoreMemberComputedLocal`.

The fused ops convert the key ONCE per evaluation and share it between
the internal read and write — mirroring the step path's
`GetMemberComputedKeep`, whose write reuses the converted key (spec
13.15.3). A decomposed read+store with the store re-deriving the key
from its slot would re-run an object key's ToPropertyKey after the
read's getters and was rejected in design (the once-key probe
`scratch/rmw_key_once2.js`, whose `toString` yields a fresh name per
call, asserts the read and the write hit the same property per
iteration under the JIT, `--no-jit`, and `--gc-stress`).

Measurement (2M-iteration certified loops, `scratch/computed_probe2.js`,
multi-run, the machine quiet): `o[k] += 1` ~158 -> ~54ns/iter and
`o[k]++` ~160 -> ~51.5, in both engines (the JIT's fused-op slow-helper
call keeps the compiled loop). The register executor's residual is the
once-per-iteration key conversion + the member-cell read and warm-store
write. A computed compound with a side-effectful RHS (`o[k] += o.y`), an
expression-position update, and constant-key forms (`o['k']`, `o[0]`)
stay on the step path — the constant-string form is the compile-time
name-normalization slice, and the numeric-keyed dense-array RMW (a
canonical runtime Number key) still takes the general (string-converted)
path, pending the fused core's numeric fast paths.

Gates: clippy clean; `cargo test --workspace` green (new lowering test
asserting the three shapes reduce to their single fused ops and that the
member-read-RHS and expression-position forms stay step-path); the edge
probe (`scratch/l1c_computed_rmw_probe.js`: every binary compound op
against a name reference, string `+=`, own getter/setter, chain
accessors, shadowed own data, non-writable, delete+recreate, logical
assigns, counter RHS, numeric-string/NaN updates, prefix/postfix,
explicit `o[k] = o[k] + 1`, expression-position values, nullish, the
once-per-evaluation object key, descriptor/order integrity, mid-loop
break) and the L1a/L1c/compound/update probes pass under the JIT,
`--no-jit`, and `--gc-stress`; the three release sweeps are identical to
the parent — language 23721/23724 (3 skip), built-ins 23657/23812
(155 skip), annexB 1086/1086, all with zero fail/crash/hang.

### Numeric-keyed array RMW fast paths in the fused core (measured 2026-09-03)

A canonical runtime Number key on a dense Array or typed array
(`arr[i] += 1`, `ta[i]++` — the byte/count-loop shape) still ran the
fused core's general path, converting the number to a string key and
re-resolving the property every iteration (~740ns/iter on a dense
array). The two fused cores now serve a canonical Number key through the
element paths directly — `array_element_get`/`array_element_write` for
dense/spilled arrays (a hole reads `None` and falls to the general path,
so a prototype-chain read still sees the chain) and
`typed_array_element_get`/`typed_array_element_set` for typed arrays
(OOB reads undefined and the setter no-ops, spec 10.4.7.5/10.4.7.6). The
numeric paths run no user code (ToPropertyKey of a Number is pure), so
they need no key conversion and are exact; when the element write doubts
after the compound's coercion mutated the receiver, the ALREADY-computed
value is written through the converted key — never re-read/re-applied.

The cores are push-neutral (`assign_member`'s result push is popped
inside): the numeric paths return without any push, so a JIT helper that
unconditionally popped would steal a caller stack slot (a compiled
`o[k]++` on a Uint8Array underflowed `do_call_fast` at the next call
until the helpers stopped popping).

Measurement (2M-iteration certified loops, `scratch/numeric_rmw_probe.js`):
dense-array `o[0] += 1` ~740 -> ~20ns/iter and `o[0]++` ~17.5ns (JIT,
interp ~28/25.5), typed-array (Float64Array) `ta[0] += 1` ~53.5 and
`ta[0]++` ~51 (interp ~65/59.5). Gates: clippy clean, workspace tests
green, the numeric edge probe (`scratch/numeric_rmw_edge_probe.js`:
dense in-range RMW, a hole reading through a prototype and writing an
own element, append-position NaN semantics, uint8 modulo wrap /
uint8clamped clamp / float64 +=, typed OOB no-op, a coercion-mutated
receiver writing exactly once, numeric-string elements) passes under the
JIT, `--no-jit`, and `--gc-stress`; the three release sweeps are
identical to the parent.

Next: the compile-time string-literal-key normalization (a literal
computed key like `o['k']` is observationally a name and should compile
to the named machinery).

### Literal-string computed keys compile as names (measured 2026-09-03)

A computed member key that is a plain string literal (`o['k']`, `o['a-b']`,
`o?.['m']`) is observationally identical to the named form: ToPropertyKey
of a string is the string itself, so both resolve through the same
atom-keyed property machinery. The parser now normalizes the AST at the two
member-access `[index]` sites (`parse_subscripts`, `parse_optional_link`;
`super['k']` stays computed) — `member_property_from_index` interns the
literal and emits `MemberProperty::Name`, so every compiler path (reads,
writes, compounds, updates, calls, `delete`, optional chains) automatically
runs the fast named machinery. Excluded: the `"length"` atom, where the
named read path's typed-array length shortcut (a slot read) differs from the
computed path's prototype accessor invocation — keeping the literal form on
the computed path preserves today's behavior for that one atom. Object
literal `{['k']: v}` properties are a separate AST (`PropertyName`) and are
untouched (`{['__proto__']: x}` stays a plain property).

Measurement (2M-iteration certified loop, `scratch/computed_probe3.js`):
`o['k'] += 1` ~157 -> ~12.5ns/iter (JIT) / ~28.5 (interp) — now identical
to the named `o.x += 1`. Gates: clippy clean, workspace tests green, the
literal-key probe (`scratch/literal_key_probe.js`: literal reads/writes/
compounds/updates, non-identifier and empty-string keys, `delete`, accessors,
optional chains, literal-key calls, `__proto__` writes, certified loops, the
typed-array `['length']` exclusion, `super['k']`) passes under the JIT,
`--no-jit`, and `--gc-stress`; the three release sweeps are identical to
the parent.

Next: the remaining literal-COMPUTED key is a NUMBER literal (`o[0] += 1`
on a dense array, ~915ns): the register RMW lowering rejects `Const` keys,
so it stays on the step path — accept `Const` keys in the computed member
read/RMW lowering (a `Const` Number key then reaches the fused core's
numeric element fast paths, and a `Const` string key is moot now that
string literals normalize to names).

### Const-key computed RMW on the register executor (measured 2026-09-03)

The register computed RMW lowering only accepted a frame-slot (`Reg`)
key, so a literal Number key (`o[0] += 1` on a dense array) stayed on the
step path: every iteration converted the number to a string key and
re-resolved the property (~915ns/iter). The fused ops
(`CompoundMemberComputedLocal`/`UpdateMemberComputedLocal`/
`StoreMemberComputedLocal`) now carry the key as a `RegOperand` — a
`tdz=false` frame slot OR a `Const` (`is_stable_computed_key`) — resolved
by the interpreter handler / JIT leaf_operand at op-execution time (a
`Const` is immutable, so the same late-read/deferred-read soundness
holds). A `Const` Number key reaches the fused core's numeric element
fast paths (dense/typed arrays) with no key conversion.

Measurement (2M-iteration certified loops, `scratch/computed_probe3.js`):
dense-array `o[0] += 1` ~915 -> ~20ns/iter (JIT) / ~29.5 (interp) — the
numeric element paths — and the explicit `o[0] = o[0] + 1` and `o[0]++`
literal forms lower too. Gates: clippy clean, workspace tests green (the
lowering test asserts the Const-key fused shape), the const-key probe
(`scratch/const_key_rmw_probe.js`: literal vs runtime-key agreement,
separate literal indices, append-position NaN, plain-object index-string
properties, explicit `o[0] = o[0] + 1`, typed arrays incl. OOB,
oversized non-index keys) and the heap-const-key probe (`o[1n]` — the
BigInt-literal key rides the `load_const` field plumbing, rooted under
`--gc-stress`) pass under the JIT, `--no-jit`, and `--gc-stress`; the
three release sweeps are identical to the parent.

Next: the computed-member family is now fast across runtime keys (string
and number), literal string keys (names), and literal number keys
(Const). The remaining measured member-op gap is the NAMED side's own
exotic cases and the L1c shapes work (the plan's structural item);
re-probe the jit-bench rows and pick the next mechanism by measurement.

### PostInc keys on a Number slot resolve on the raw f64 (measured 2026-09-03)

The buildString rows' interpreter column is the computed-store machinery:
`a[l++] = i` runs one `RunRegBody` op whose `PostInc` key resolution went
through the general `update_value` (a full `to_numeric`
ToPrimitive/ToNumber dispatch) on every iteration just to add 1 to a
slot that provably holds a Number. The register executor now handles a
`PostInc` key whose slot holds a Number inline (`n ± 1`, one slot write,
the old value yielded unchanged) and falls through to `update_value` for
everything else. Sound because Number is closed under `++`/`--` (NaN and
the infinities stay put) and `to_numeric` of a Number is the value
itself. Interpreter-only: the JIT resolves `PostInc` in machine code, so
no JIT arm and the compiled columns are untouched.

Measurement (interleaved parent/child A/B, 3 rounds each, 2026-09-03):
`buildString shape` interp ~171-192ms -> ~145-162ms (best-of-3 171.3 ->
145.0, ~15%), JIT column unchanged (~40ms); the isolated `a[l++]=i`
append drops ~41.0 -> ~33.2ns/iter interp (~19%), while the
step-path `a[l]=i; l++` control (a separate `UpdateLocal` step, not the
fused key) is unchanged; `buildString full` (apply/fromCodePoint-bound)
is unchanged. Gates: clippy clean, workspace tests green (new
`post_inc_key_number_fast_path_matches_update_value` regression covering
NaN/±Infinity/-0/fractional/over-2^32 keys and the BigInt fallback),
the computed-RMW probe set passes under the JIT, `--no-jit`, and
`--gc-stress`; the three release sweeps are identical to the parent.

Separate pre-existing finding (reproduced at parent HEAD, not introduced
here): a register computed store whose `PostInc` key is NaN or Infinity
raises an illegal instruction under the JIT — the compiled body's
non-canonical-Number key falls through to the slow helper, and the
machine-code fallback crashes (the interpreter is exact; `--no-jit`
runs the same shape cleanly). Left for a JIT-side landing.

Next: the remaining interp cost on the append store is the object
machinery itself (chain-clean verdict, buffer push, length mirror,
generation bump) — the plan's structural L1c element-storage item; the
step-path `if (l === N)` guard overhead (~16ns/iter on the shape row) is
the second measured term, reducible only with a completion-elision or
fused-test landing.

## Deferred milestones

Each milestone is deferred with its gate from PLAN Phase 18. A milestone is
"done" only when it passes its gate; none are correctness gates.

| Milestone | Gate | Status |
|---|---|---|
| NaN-boxed `Value` (u64 with tag fast paths) | arithmetic micro-benchmark ≥ 2x vs snapshot | **Done** — correctness landed (migration below) and the shapes work closed the gate: real-loop arithmetic is ~2.2x the corrected baseline. |
| Bytecode VM replacing the tree-walker | `--print-bytecode` dumps real bytecode; hot-path bench ≥ 5x | **Done (2026-09-01)** — everything compiles and runs on the `Vm` (`--print-bytecode` prints the compiled `Step` stream, and the gate is met by one to two orders of magnitude: arithmetic 2.52s → ~15.2ms (~166x), property ~28.3ms (~114x), array iteration ~25ms (~617x), function calls ~27.5ms (~208x) vs the 2026-08-18 corrected baseline). The ≥5x gate that was "still open" closed with the GC-5 `Copy`-value win and the JIT-era interpreter cuts. |
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
  `payload` in bits 43-0 (the `Rc` pointer shifted right 4 — the
  16-byte-aligned allocation base, so a 48-bit address space).
- **Doubles** — every other bit pattern, preserved exactly: signaling NaNs
  and quiet NaNs with bits 50-48 ≠ 0 survive as-is. A quiet NaN with bits
  50-48 = 0 collides with the tag region and is canonicalized on box to
  `0x7FF9_0000_0000_0000`; this is unobservable from JS (no NaN-payload
  introspection), and the `DataView`/`Float64Array` fixtures stay green
  (verified by the full sweep).
- **Tags** — `0x0` undefined, `0x1` null, `0x2` false, `0x3` true, `0x4`
  BigInt, `0x5` String, `0x6` Symbol, `0x7` Object, `0x8` Function;
  `0x9`-`0xF` reserved. Payload capacity is 44 bits (17.6 TB) of shifted
  pointer, i.e. a 48-bit address space — far above any real `Rc`
  allocation.
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

## Bytecode VM milestone (Cut 1-4 + script-level bindings + evaluator fast paths landed, gate met 2026-09-01)

The tree-walker is gone from normal execution: every expression and
statement compiles to `Step` bytecode at creation (`compile_expr`/
`compile_statements` in `crates/runtime/src/ir.rs`), and a `Vm` dispatch
loop runs the compiled body for ordinary calls/constructs, generators,
async functions, and top-level scripts. Cut 3 gives simple-param
functions and arrows frame-slot bindings; Cut 4 fuses the loop test and
update into slot ops and adds a primitive fast path to the relational
evaluator; the script-level binding fast path reads declared top-level
vars directly off the global object; and the evaluator numeric fast paths
hoist the number-number check above the ToNumeric/ToPrimitive round-trips
in `apply_binary`'s arithmetic/bitwise paths. All at zero conformance
regressions.

The batching defaults that cloned
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
lost. The ≥5x gate (measured against the later, already-optimized walker
baseline in `bytecode-plan.md` — arithmetic 1.14s → the plan's "≤0.23s") is
met by a wide margin as of 2026-09-01; the interpreter rows vs that
baseline:

| Benchmark | walker baseline | now (2026-09-01) |
|---|---|---|
| arithmetic | 1.14s | ~15.2ms (~75x) |
| property access | 1.50s | ~28.3ms (~53x) |
| string concat | ~0.15s | ~4.0ms (~38x) |
| array iteration | 13.6–15s | ~25.0ms (~550x) |
| function calls | 4.2–4.4s | ~27.5ms (~155x) |

The gate closed with the cuts since the early bytecode work (fused loop
heads, register bodies, the raw-f64 loop counter, the member/element
value caches) plus the GC-5 `Copy`-value win; the interpreter rows are
now 15-30ns/iter on the hot loops (see the Current status and floor
sections).

## GC milestone (PLAN Phase 18 item 2) — landed, perf gate closed 2026-09-01

The plan's GC milestone (arena heap + mark-sweep; root tracing;
ephemeron-aware WeakMap/WeakSet; `WeakRef`/`FinalizationRegistry` semantics
activated; `--gc-stress` mode) is a rewrite of the value/object model from
`Rc`-based ownership to GC-managed handles. It is **landed** (GC-1..4:
`Handle` → GC heap, collector wiring, per-allocation `--gc-stress` root
audit, ephemerons, weak-ref semantics) — see `docs/gc-plan.md`.

**GC-5 measured (2026-08-26)** — the eight `--bench` rows vs the pre-GC Rc
model on the same machine (interleaved medians):

| Row | Rc model | GC model | delta |
|---|---|---|---|
| arithmetic | ~25ms | ~12.5ms | ~2.0x faster |
| property access | ~58ms | ~28ms | ~2.1x faster |
| array iteration | ~52ms | ~23ms | ~2.3x faster |
| function calls | ~45ms | ~23ms | ~2.0x faster |
| closure capture | ~53ms | ~25ms | ~2.1x faster |
| per-iteration | ~16.7ms | ~8.8ms | ~1.9x faster |
| string concat | ~10.7ms | ~20ms | ~1.9x slower |
| construct churn | ~36ms | ~74ms | ~2.1x slower |

The GC delivered the predicted ~2x on the machinery rows (the `Copy`-value
win removes Rc clone traffic), but the allocation-bound rows (construct
churn, string concat) regressed ~2x: `Gc::new` is heavier than `Rc::new`
(the live-set registration + stress checks), and mark-sweep reclaims a
loop's garbage in one batch at the script boundary (inside the timed
window) instead of per iteration. The plan's slot-arena allocation (the
original recommendation) is **deprioritized**: `gc-plan.md` GC-5 measured
the free-list half net-neutral and attributes the remaining construct/
concat gap to the engine hot path (the 16-23x gap to V8's interpreter is
hot-path, not collector, cost). The hot-path cuts since then have closed
the allocation-bound rows anyway — construct churn is now ~17ms and
concat ~4ms on `--bench`, ~2x FASTER than the Rc model's 36ms/10.7ms —
so the GC perf gate is met (via the engine cuts, not the collector).

## Accepted no-op CLI flags

`--stack-size` and `--max-old-space` are accepted by the CLI for
compatibility. They are no-ops because the corresponding machinery (call-
stack depth control, a heap to cap) does not exist yet. (`--print-bytecode`
is live — it prints the compiled `Step` stream via
`runtime::ir::debug_print_body`.)
