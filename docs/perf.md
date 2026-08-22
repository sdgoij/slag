# Performance

Current performance state of the slag runtime and the PLAN Phase 18
performance milestones, each behind a benchmark gate rather than a
correctness gate.

## Current architecture

- **Value representation**: a NaN-boxed `u64` (PLAN Phase 18): a quiet-NaN
  tag region (top 16 bits `0x7FF8`) holds a 4-bit tag plus a 44-bit `Rc`
  payload for the heap variants; every other bit pattern is a double stored
  exactly. The payload holds the allocation pointer shifted right 4 (the
  `RcBox` base is 16-byte aligned), so a full 48-bit address space round-
  trips. Heap values own one strong `Rc` ref; `Clone`/`Drop` reconstruct
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

### Current status (measured 2026-08-19)

The bytecode VM work — Cut 1-4, the script-level binding fast path, the
fused loop test/update ops, the relational fast path, and the evaluator
numeric fast paths — is at zero conformance regressions. Against the
corrected baseline:

| Benchmark | corrected baseline | today | speedup | 5x target |
|---|---|---|---|---|
| arithmetic | 2.52s | ~1.05s | 2.4x | 0.50s |
| property access | 3.22s | ~1.30s | 2.5x | 0.64s |
| string concat | 0.88s | ~0.16s | 5.5x — gate met | — |
| array iteration | 15.42s | ~13.5s | 1.1x | 3.08s |
| function calls | 5.73s | ~1.66s | 3.4x | 1.15s |

(The `bytecode-plan.md` gate used a later, already-optimized walker
baseline — arithmetic 1.14s → the plan's "≤0.23s"; this table is the
documented 2026-08-18 corrected baseline, which the bytecode work has
moved 2.4-3.4x on the hot benches.)

The two hot gates are arithmetic (needs ~2.1x more) and function calls
(~1.4x). The measured cost model (bytecode-plan.md §7): top-level loops
are bound by global-object property access (~5 accesses/iteration at
~100-200ns each) and fast-function loops by the binary evaluator
(`apply_binary`/`abstract_relational` at ~20-40ns vs ~2-5ns for
mechanical steps). The evaluator numeric fast paths have landed — the
number-number check now sits above the ToNumeric/ToPrimitive round-trips
in `apply_binary`'s arithmetic/bitwise paths and in `abstract_relational`
(measured ~6% on the top-level arithmetic bench, and ~25% on the
fast-function loop shape — 0.166s → 0.125s, where `apply_binary`
dominates; 3-run medians) — so the remaining levers are a fast
global-property access (cell/IC) and less per-call machinery (a fast
call still pushes a full `ExecutionContext` and bumps ~10 `Rc`
refcounts).

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

## Bytecode VM milestone (Cut 1-4 + script-level bindings + evaluator fast paths landed, gate open)

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
lost, and function calls have moved ~2.5x vs the walker; the ≥5x gate is
not met:

| Benchmark | walker baseline | Cut 2 (HEAD) | now |
|---|---|---|---|
| arithmetic | 1.14s | 1.19s | ~1.05s |
| property access | 1.50s | 1.59s | ~1.30s |
| string concat | ~0.15s | 0.160s | ~0.16s |
| array iteration | 13.6–15s | 14.1s | ~13.5s |
| function calls | 4.2–4.4s | 4.56s | ~1.66s |

The runs are noisy on this machine (±15%). The measured cost model
(bytecode-plan.md §7) says arithmetic is bound by global-object property
access (the walker's arithmetic was already fast) and fast-function loops
by the binary evaluator. The evaluator numeric fast paths landed (~6% on
arithmetic, 3-run medians); the gate's arithmetic target still needs a
fast global property access (cell/IC), and function calls need less
per-call machinery.

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
