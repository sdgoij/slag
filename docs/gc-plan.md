# GC milestone: implementation plan

This is the engineering spec for replacing the `Rc`-based ownership model
with a garbage-collected heap (PLAN.md §4.3 step 2, Phase 18 item 2, and the
deferred milestone in `docs/perf.md`). The regression net is the same one
every milestone uses: `cargo clippy --workspace --all-targets -- -D warnings`
clean, `cargo test --workspace` green, and the full release test262 sweep at
zero regressions on the previously-passing union, at every cut.

Status: GC-0 (spike + baseline) done — `--gc-stress` no-op, leak harness,
and the Weak-* fixture enumeration are landed with the measured baseline
(see GC-0). The observable spec surface (`WeakRef`, `FinalizationRegistry`,
`keptAlive`, `WeakMap`/`WeakSet` keying) is implemented and tested; only the
collector is missing.

## 1. What the milestone buys

- **Correctness** — closes the documented `Rc` cycle leaks (long-running
  processes leak; `WeakRef.deref()` never returns `undefined`;
  FinalizationRegistry cleanup never fires; WeakMap/WeakSet entries are
  never collected).
- **Performance** — `Value` becomes `Copy` (a plain NaN-boxed `u64`), the
  stated prerequisite for the three remaining allocation-bound benchmark
  rows (construct churn ~12.9x, string concat ~8.6x, per-iteration closures
  ~6.8x behind V8 `--jitless`; the `docs/perf.md` 2–4x target for those rows
  is explicitly "after GC").
- **Fidelity** — the Weak-* fixtures that currently pass *because nothing
  dies* become real assertions.

## 2. The current model (what is being replaced)

- `Handle<T> = std::rc::Rc<T>` (`crates/crux/src/handle.rs`) — the documented
  seam; the arena swap is supposed to happen behind this alias.
- `Value(u64, PhantomData<Rc<()>>)` (`crates/crux/src/value.rs`) — NaN-boxed;
  the box holds a raw `Rc` pointer; the `PhantomData` makes it `!Copy`,
  `!Send`, `!Sync` (agent-local by construction).
- `ValueKind` holds `Handle<BigInt>` / `Handle<JsString>` / `Handle<Symbol>` /
  `Handle<JsObject>` / `Handle<Function>`; objects, realms, and environments
  are all `Handle<T>` everywhere in `crux` + `runtime`.
- `JsString` is `Flat(Arc<[u16]>)` or `Rope { left/right: Handle<JsString>, …
  , flat: OnceLock<Arc<[u16]>> }` — rope children are handles; the flat cache
  is an external `Arc`.
- `SharedBuffer` is deliberately `Rc<RefCell<Vec<u8>>>` (single agent) or
  `Arc<[AtomicU64]>` (`workers` feature) — shared-memory buffers stay
  refcounted, never GC-managed.
- Weak semantics are stubs: the agent's `weak_ref_targets` map is *strong*
  (nothing dies), `finalization_registries` cells never enqueue, WeakMap/
  WeakSet entries are strong.

## 3. Roots the collector must trace

Precise (JS-visible) roots, per PLAN §4.3 plus the code:

- `Agent::execution_context_stack` (each `ExecutionContext`'s realm,
  lexical/variable/private environments, function, source)
- Realm, global environment, intrinsics table, global object
- Job queues (`promise_jobs`, `generic_jobs`, `timeout_jobs` — the closures
  capture `Value`s)
- Module records and pending Promise reactions
- Every live `Vm`: `stack: Vec<Value>`, `frame: Frame`, `leaf_frame_base`
  leaf segments, the destructure stacks (the VM holds raw `Value`s)
- The agent's IC value caches (`member_value_cells`,
  `array_element_value_cells`, `array_length_value_cells`, …) — trace or
  invalidate on collection; the index-only caches (`global_cells`,
  `member_cells`, …) need no tracing
- `weak_ref_targets` / `finalization_registries` (as weak tables after
  GC-4), `error_stack`/`error_data`, annex-B hoistables
- The `ffi`/`jsc` opaque-ref tables: a `JSValueRef` held by host C code must
  pin its object (thread-local handle tables become roots)

Rust-side locals and closure captures holding `Handle<T>` are handled by the
rooting strategy below, not by precise tracing.

## 4. Preflight decisions

### D1 — Collector: custom arena + mark-sweep (recommended)

PLAN §4.3 step 3 says to evaluate existing crates (`gc`, `broom`) before
committing. Recommendation: a **custom arena (`Vec<Slot>` + free list) and a
mark-sweep collector**, modeled on the `gc` crate's design (thread-local
heap, conservative stack scan), because the codebase needs ephemeron-aware
WeakMap/WeakSet, a `--gc-stress` mode, and precise tracing of the VM value
stacks — none of which a general crate provides off the shelf, and the
project's self-contained ethos rules out a large dependency. Final decision
is made at the end of GC-0.

### D2 — Rooting: conservative native-stack scan (recommended)

A precise collector cannot see the thousands of Rust locals and closure
captures that hold handles. Explicit Ruffle-style `root()`/`unroot()` would
be an invasive API change across the whole runtime. Recommendation: trace
the JS-visible roots precisely, and **conservatively scan the native stack
for arena indices** (`gc`-crate approach). Imprecise is safe: it may retain
garbage, never use-after-free. The `Value` tagging must make object indices
recognizable to the scanner (decided in GC-1).

## 5. Cuts (each ends clippy-clean + workspace-green + zero sweep regressions)

### GC-0 — spike and baseline (no engine change) — DONE

- `--gc-stress` plumbed as a documented no-op (`crates/cli`, accepted
  alongside `--stack-size` / `--max-old-space` / `--harmony-*`), with a
  parse test.
- Leak-detection harness landed: `cargo run --release -p cli --bin leak
  <cycle|chain>` runs a workload in-process and samples the working set
  (`crates/cli/src/bin/leak.rs`; `tasklist` on Windows, `/proc/self/status`
  elsewhere). **Baseline (Rc model, 200k iterations):**

  | workload | start | end | growth |
  |---|---|---|---|
  | `cycle` (`o.self = o`) | 8,676 KB | 146,812 KB | **+138,136 KB** (linear, ~0.69 KB/iter — one leaked object graph per iteration) |
  | `chain` (8-node acyclic list) | 8,400 KB | 8,612 KB | +212 KB (flat — Rc frees acyclic memory) |

- **Weak-* fixture enumeration:** the pinned corpus has **no
  collection-forcing fixtures** — all 302 (29 `WeakRef` + 47
  `FinalizationRegistry` + 141 `WeakMap` + 85 `WeakSet`) are API-surface
  tests and pass today; zero use a `$262.gc()`-style hook and none assert
  `deref() === undefined` or cleanup firing. GC-3/GC-4 validation is
  therefore: existing tests stay green, new runtime unit tests exercise
  collection through a force-gc test hook, and the `--gc-stress` sweep gate
  passes — there is no corpus fixture set to un-skip.
- **Decisions recorded:** D1 — custom arena + mark-sweep, modeled on the
  `gc` crate (thread-local heap, conservative stack scan); D2 — precise
  tracing of the JS-visible roots plus a conservative native-stack scan
  for Rust-held handles.
- Gate: harness runs, baseline recorded — met (above).

### GC-1 — arena heap, `Handle` as index, `Value` becomes `Copy`, first collector (the big cut)

**Slice 1 — the crux heap module (landed).** `crates/crux/src/heap.rs`:
`Trace` trait, `Gc<T>` (`Copy`, `!Send`/`!Sync`, derefs straight to its box),
`GcCell<T>` interior mutability, and a thread-local mark-sweep `Heap`
(cells are `GcBox<T>` with a mark bit, registered in the live set; collect
marks from the passed roots and sweeps the rest). Cycle collection, root
retention, idempotence, and acyclic reclamation are unit-tested. Precise
roots only for now — the runtime roots and the conservative native-stack
scan land in the following slices.

**Remaining slices (after slice 3).** Thread-local arena refinement — `Handle<T>`
  as a `Copy` index with `Deref` through a thread-local accessor (handles are
  still direct box pointers today); `RefCell` fields move to a GC-cell
equivalent; the sweep pushes dead cells to a free list instead of `Box`-
freed allocations.
- `Value` drops `PhantomData<Rc<()>>` → `Copy` (a plain `u64`), kept
  agent-local with a `!Send` marker. This is the perf unlock (GC-5).
- Ropes: children are GC handles already (slice 2); the `OnceLock<Arc<[u16]>>`
  flat cache stays an external `Arc`. `SharedBuffer` stays `Rc`/`Arc`.

**Slice 2 — the `Handle` flip (landed).** `Handle<T>` is now `Gc<T>`: a `Copy`
pointer into the (still-unwired) GC heap, behind the same alias so call sites
barely changed. Scope measured:

- **14 types need `Trace` impls** (the heap-edge enumeration): `JsObject`,
  `Function`, `JsString` (rope children), `Symbol`, `BigInt`, `Realm`,
  `SourceTextModule`, `TypedArraySlots`, `PrivateEnvironment`, `EnvRecord`,
  `ScriptRecord`, `ArgumentsSlots`, `ModuleNamespaceSlots`, `CruxObject`.
- **641 allocation sites** (`Handle::new`/`Rc::new`) → `Gc::new`;
  ~260 `Handle<…>` annotations; `Handle::clone`/`Rc::clone` become copies.
- **`value.rs` refcount bookkeeping disappears**: `Value` stores the box
  pointer (16-byte aligned, same payload shape as today) and needs no
  `Rc::from_raw`/`into_raw` in `Clone`/`Drop` → `Clone` is a plain pointer
  copy. `Value` stays `Clone` (not `Copy`) with its `PhantomData<Rc<()>>`
  `!Send` marker; the `Copy` upgrade is the GC-5 perf unlock (see the note
  in `crates/crux/src/value.rs`).
- **`Rc::ptr_eq` (17 sites: module.rs/ir.rs/module_source.rs)** → `Gc::ptr_eq`.
- **No `try_unwrap`/`make_mut`/`downgrade`/`strong_count` anywhere** — the
  feared semantic reworks don't exist; interior mutability is already
  `RefCell`-based. `SharedBuffer` and the agent's `FinalizationData` stay
  `Rc`/`Arc` deliberately.
- The flipped-but-unwired heap never collects, so the tree stays leaky-but-
  safe; the collector wiring (slice 3) is what makes `--gc-stress` live.

**Status.** Workspace compiles, `cargo test --workspace` green (~4,323
pass), clippy `-D warnings` clean, and the full release sweep is
48,025 pass / 0 fail / 158 skip / 439 hang (baseline 48,006 / 0 / 158 /
458; the hang→pass delta is load-dependent classification wobble, see the
`slag-conformance` skill). The leak harness confirms the leaky-but-safe
state: `cycle` grows unboundedly (≈690 MB @ 200k iters, as under `Rc`), and
`chain` now grows too (≈2.4 GB @ 200k iters) — the tracing heap reclaims
nothing until slice 3 wires collection, so the old `Rc`-era "acyclic
structures free on drop" property is gone until then.

**Slice 3 — collector wiring (landed).** The heap now collects from the
precise JS-visible roots plus a conservative native-stack scan:

- **`Agent::trace_roots`** (`crates/runtime/src/agent.rs`) visits every
  `Value`/`Handle`/`JsString`/`Symbol` the agent holds directly — the
  execution-context stack, the realm/module tables, the promise/async/
  generator/iterator/disposable auxiliary states, the IC value caches, the
  pooled `Vm`s, `kept_alive`, the Weak-* tables (strong until GC-3/4), and
  the leaf caches. The 14 heap types' `Trace` impls (slice 2) plus ~35 new
  auxiliary `Trace` impls (job, promise, async, module, Intl/Temporal
  records, `Vm` and its suspended stacks, `EcmaFunction` incl. the compiled
  body's embedded literal `Value`s, `Completion`, `Reference`, …) cover the
  reachable graph.
- **Conservative native-stack scan** (`Heap::collect_with_stack`): every
  word in the current thread's committed stack region is matched against the
  live box set — a raw `Gc<T>` word or a NaN-boxed `Value` payload
  (`Value::encoded_box_address`) — so Rust locals and closure captures
  survive. Stack bounds come from `GetCurrentThreadStackLimits` (Windows)
  or `/proc/self/maps` (Linux); other platforms run precise-roots-only.
- **Marking is iterative** (an explicit worklist): a deep rope or prototype
  chain cannot overflow the native stack (the recursive first cut crashed
  the `String.prototype.repeat`/`replace` sweep fixtures).
- **Trigger**: `Agent::maybe_collect` fires at safe points — script
  boundaries and job-queue drains with no pending jobs (job closures are
  opaque to tracing) — when the live count doubles past the post-collection
  baseline, and at every safe point under `--gc-stress` (wired through the
  CLI, no longer a no-op).

**Status.** All three gates met: `cargo test --workspace` green (~4,325
pass incl. the 3,324 test262 fixture tests), clippy `-D warnings` clean,
and the full release sweep is 48,023 pass / 0 fail / 158 skip / 441 hang
with **0 failures and 0 crashes** (baseline 48,006 / 0 / 158 / 458 — the
hang→pass delta is the documented load-dependent wobble). The leak harness
now bounds both workloads: `cycle` oscillates ~9–13 MB @ 200k iters
(previously +690 MB linear) and `chain` ~9–13 MB (previously +2.4 GB) —
the cycle leaks are closed, the headline correctness win. The root set was
hardened during bring-up: the compiled-body literal `Value`s in
`EcmaFunction.ir` and the `Vm` suspended stacks were the gaps the first
sweeps caught. GC-2 (`--gc-stress` across the sweep) is the remaining net
for missed roots.

### GC-2 — root audit and `--gc-stress` hardening

**Increment 1 (landed).** The per-allocation stress net is wired and already
caught three real root gaps:

- **Per-allocation collection**: `Gc::new` triggers a full collection with
  the fresh box as an extra root (it is not yet reachable from any handle).
  The runtime registers a thread-local collector that finds the current
  agent via the `with_agent` TLS window; outside an agent window (realm
  bootstrap) it is a no-op. `test262-sweep --gc-stress` propagates the flag
  to every fixture agent.
- **Active Vms and their compiled bodies are precise roots**: a mid-
  execution collection must trace the running Vm (its heap-buffered value
  stacks are invisible to the stack scan) and the body it is executing (a
  script body is a per-evaluation `Rc<CompiledBody>` whose steps embed
  literal `Value`s — the gap that corrupted `str += "."` loops). Nested
  calls and tail calls are tracked through a thread-local run stack.
- **Fail-safe tracing under in-flight borrows**: a traced `RefCell` that is
  mutably borrowed mid-collection aborts the sweep (retain everything)
  instead of panicking — imprecise, never a use-after-free.
- **Opaque job closures**: queued (and the running) `Box<dyn FnOnce>` job
  closures hold captured `Value`s no precise `Trace` can reach; their
  allocations are scanned conservatively (`Layout::for_value`), the same
  mechanism that fixed the flaky promise UAF.

**Remaining.** The full `--gc-stress` sweep gate (slow; per-allocation
collection is O(live) per allocation), a rare residual flake in the
deep-concat stress path, and the rest of the §3 audit (every agent table
holds `Value`s; the remaining un-traced `Rc<RefCell<…>>` auxiliary states
must be verified against the stress sweep). Gate: `--gc-stress` clean
across the sweep; leak harness bounded (the harness is already bounded).

**Increment 2 (landed).** The built-ins and language crash clusters are
closed — the full `all` sweep is `--gc-stress` clean: **0 fail / 0 crash /
158 skip** across 48,622 fixtures. The remaining ~539 hangs are all
pre-existing stress-cost artifacts (the RegExp property-escape table
builds, the decodeURI/encodeURI/parseInt/parseFloat families, and a few
allocation-heavy intrinsic-graph walks — each passes standalone under a
long deadline; the live set stays bounded). Root gaps closed:

- **Native buffers across allocating loops**: `Atomics.notify`'s
  same-thread resolve drain, `Function.prototype.apply`'s argument list
  (build *and* the callee call), `TypedArray` filter/from element
  collection, `Object`/`Map.groupBy` (the shared `group_by` loop *and* the
  result-object/map build — the guard must span both or the gap between
  them is sweepable), `DisposableStack` move/dispose resource Vecs, and
  `JSON.parse`'s ParseRecord tree all get `StressSuppress` windows so
  half-built `Vec<Value>` buffers cannot be swept.
- **`new_promise_capability`'s captured resolving functions**: the
  executor writes them into a native `Rc<RefCell>` while the user
  constructor body still runs — suppress the construct window.
- **Mapped-arguments accessors hold GC handles**: the MakeArgGetter/
  MakeArgSetter closures captured the parameter `JsString` and
  `EnvRef` — both sweepable. The name is now interned (immortal) and the
  environment is rooted from `ArgumentsSlots::env` (an opaque `GcAny`
  edge), so the closure captures stay valid exactly as long as the
  arguments object; no per-call agent table (an earlier design retained
  every called function's environment and blew up the zip fixtures to
  minutes under stress).
- **`Vm::run_abrupt` ran unrooted**: the abrupt-completion handling
  (closing suspended destructure iterators, `throw_machinery`) allocates
  *before* `run_inner` registers the Vm in the active-run stack, so a
  suspended async-generator resume could sweep its own `async_for_of_stack`
  mid-close (the `async-gen-decl-dstr-array-elem-iter-rtrn-close-null`
  crash family). The Vm is now registered for the whole `run_abrupt` call.

Gate status: `--gc-stress` clean across the sweep; leak harness bounded.

### GC-3 — ephemeron-aware WeakMap / WeakSet

**Landed.** The weak tables now have true ephemeron semantics instead of
strong entry storage:

- **Collector fixpoint** (`crux/heap.rs`): traces register ephemeron edges
  (`note_ephemeron(key, value)`); the mark phase promotes a value once its
  key is marked, iterating to a fixpoint (a value that is itself a weak key
  can promote further edges). `collect`/`collect_with_stack` now return the
  swept box addresses.
- **Runtime wiring** (`runtime/agent.rs`): `trace_roots` registers
  WeakMap key→value and WeakSet element→element edges instead of tracing
  the tables strongly; `collect_garbage_with` compacts the weak tables
  after each collection, dropping entries whose key (or element) box was
  swept — a dead key's handle can never dangle on the next access.
- **`$262.gc()`**: the test262 host hook forces a full collection, making
  the ephemeron semantics observable to fixtures.
- Tests: `weak_map_ephemeron_lifetime` / `weak_set_ephemeron_lifetime`
  (live keys keep their values across a collection; an unreferenced key's
  entry — key and value — is swept; the live entry still works after).
- Gate: the 226 pinned WeakMap/WeakSet fixtures pass under `--gc-stress`;
  the full `all` sweep stays 0 fail / 0 crash; the leak harness stays
  bounded. (The pinned corpus never calls `$262.gc()`, so the visible gate
  is the API surface + no regression — the ephemeron machinery is verified
  by the unit tests and `$262.gc`-driven fixtures on a corpus that forces
  collection.)

### GC-4 — WeakRef and FinalizationRegistry activation

- `weak_ref_targets` becomes a true weak table (`deref()` returns
  `undefined` post-collection; `KeepDuringJob` semantics); registry cells
  enqueue cleanup jobs via `HostEnqueueFinalizationRegistryCleanupJob` with
  correct `heldValue` / unregister-token lifetimes.
- Un-skip those fixtures.
- Gate: fixtures pass; `--gc-stress` clean.

### GC-5 — the perf payoff

- Re-measure the three allocation-bound rows (construct churn, string
  concat, per-iteration closures). `Copy` values + arena handles remove Rc
  clone/alloc traffic from closure captures and constructs.
- Gate: those rows into the `docs/perf.md` 2–4x band.

### GC-6 — threads, workers, and the C-API surface

- Confirm `Value` stays `!Send` (agent-local), the `workers` feature
  (SharedArrayBuffer `Arc<[AtomicU64]>`) still compiles, and the `jsc`/`ffi`
  handle tables root host-held refs correctly.
- Gate: workers tests green.

## 6. Risk register

1. **GC-1 is a genuinely big cut.** PLAN's "the `Handle<T>` API is kept so
   the swap is mostly internal" is optimistic: `Value`'s `!Copy`-ness is
   load-bearing across the VM, and moving from `Rc` ownership to arena
   indices changes what "holding a handle" means in job closures and IC
   caches. This is why GC-2 (`--gc-stress`) must land immediately after
   GC-1 — it is the net that catches missed roots.
2. **Job-queue closures capture `Value`s.** The `FnOnce(&mut Agent)` closures
   are opaque to a precise collector; they must either stay rooted (kept
   alive for the job's short lifetime) or be converted to carry traceable
   records. Decide in GC-1, verify in GC-2.
3. **Conservative scan imprecision.** May retain garbage (fine); must never
   misidentify a non-index as an index (the `Value` tag bits decide this).
4. **Interior mutability.** `Rc<RefCell<…>>` patterns on hot objects need a
   GC-cell equivalent without re-borrow panics on the sweep path.
5. **Perf regression risk.** Arena derefs go through a thread-local indirection;
   the `Copy`-value win must outweigh it. GC-5 measures; if a hot path
   regresses, the frame-slot/leaf-inline contracts (see the
   `slag-bytecode-vm` skill) are the places to recover it.

## 7. Validation per cut

- `cargo clippy --workspace --all-targets -- -D warnings` clean;
  `cargo test --workspace` green.
- Full release sweep at zero regressions on the previously-passing union.
- From GC-2 on: `--gc-stress` clean across the sweep, and the leak harness
  shows bounded live-object counts on cyclic workloads (`cycle` flattens).
- GC-3/GC-4 keep the existing 302 Weak-* fixtures green (none force
  collection in the pinned corpus; see GC-0) and add runtime unit tests
  that exercise collection through a force-gc test hook.
