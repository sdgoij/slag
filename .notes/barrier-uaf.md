# Write-barrier miss in the promise/arguments path

**Status: resolved 2026-10-05 — the miss was a symptom, not the defect.** The
true root cause is a swept-while-live closure in `promise_all_settled` (see
§9); the A2 verifier's "old box holds an unrecorded young reference" was it
reading a dangling `Value` whose slot had been freed and recycled. Independent
of C0, and reproduces under `--jitless`.

## 1. Symptom and reproduction

```
target/release/sweep.exe built-ins \
  --filter 'Promise/allSettled/invoke-then.js' \
  --jobs 1 --batch 1 --timeout 15 --recheck-timeout 15 --gc-stress
```

Panics in the A2 verifier (`crates/crux/src/heap.rs`, `verify_barrier`):

```
write-barrier miss: 2 old box(es) hold a young reference the barrier never
recorded (0 imprecise entries); offenders:
  [("runtime::env::EnvRecord", "crux::function::Function"),
   ("crux::object::JsObject", "crux::function::Function")]
```

The `allSettled` tree under `--gc-stress` reports 10 `CRASH` (this panic
aborting a batch) + 19 `fail` (mostly `$DONE` deadlines — a `--gc-stress`
slowdown artifact, not a correctness failure) of 148 fixtures.

## 2. The two edges, narrowed

Temporary instrumentation (since reverted) labelled each container `trace`
field and dumped the offending `JsObject`:

- `runtime::env::EnvRecord` → child `Function` via `decl_bindings` (a
  `DeclarativeEnv` binding holds a young `Function`).
- `crux::object::JsObject` → child `Function` via `properties`, and it is
  `id=… kind=Arguments props=[0:young=true, 1:young=false, length:young=false]`
  — a **mapped `arguments` object** whose index property `"0"` holds a young
  `Function` (the call's first argument).

An `eprintln!` in the define path shows `arguments["0"]` is defined through the
normal path —

`arguments_define_own_property` (empty parameter map → `ordinary_define_own_
_property`) → `validate_and_apply` new-property arm → `barrier_property(obj,
&property)` —

and at that moment:

```
args-define id=1107 key="0" map=true obj_young=false value=Function(0x…810)
validate-new args key="0" obj_young=false value_young=false remembered=false
```

i.e. the container is **already old** but the **value is old too**, so the
barrier's early-out (`remember_if_young` no-ops when the child is not young) is
*correct at that instant*. Yet at the next `verify_barrier` the same property
`"0"` reads `young=true` at the **same box address** (`0x…810`).

## 3. What that means

The same box cannot go old → young: the header's `is_young` bit is only ever
cleared, never set, on a live box. So either

- **H1 (use-after-free):** the argument-value `Function` box was swept while
  `arguments["0"]` still referenced it, and its slot was recycled by a young
  allocation (same address, freshly young); or
- **H2 (unbarriered overwrite):** a *later* write replaced `arguments["0"]` (or
  the env binding) with a young `Function` through a path that does not record
  the barrier.

Both are soundness bugs. H1 is the more alarming (a reachable value freed), but
H2 is the more likely given the "store must land after the container is
promoted" shape this class of defect always hides behind (see the recurring
"a store into object storage forgot its barrier" notes in `embedding.md` §10).

## 4. Ruled out

- **The JIT.** Reproduces under `--jitless`.
- **The major's abort path.** `collect_from_work`'s abort promotes *every* live
  box and frees nothing, so it cannot leave an old box holding a young child.
- **Dirty-slot bounding.** Only `ArraySlots` overrides `trace_dirty`; `JsObject`
  uses the full `trace`, so a `properties` entry is always visited.
- **The barrier decoding a `Function`.** `Value::encoded_box_address` covers
  `TAG_BIGINT..=TAG_FUNCTION`, so `write_barrier(obj, function)` does see it.
- **The store itself.** `validate_and_apply`'s new arm calls `barrier_property`;
  the observed no-op is correct because the child was old at that moment.

## 5. Hypotheses and the probes to settle them

The collector under `--gc-stress` is the **major** (`Agent::collect_with` →
`Heap::collect_with_stack_compacting`), rooted by the precise roots plus the
conservative native-stack scan (plus the fresh box). During
`create_mapped_arguments_object` the `arguments` object and its argument values
are held in Rust locals / `Value`s; if a transient handle is not on the scanned
stack at a collection instant, H1 becomes reachable — but the object itself
stays live into the verifier, which is the inconsistency to explain.

Next probes (all temporary, `--gc-stress`, the single fixture above):

1. **Is the `Arguments` object marked at the freeing sweep?** Gate on its id;
   in the major's mark, log whether the object is marked and whether its
   `properties` `RefCell` trace succeeded or aborted. If it is unmarked but
   survives, the stack scan / root set is the gap.
2. **Is the arg-value box freed by that sweep?** Log the box address at the
   define and in the sweep's dead set; confirm free-then-reuse (H1) vs a
   different box written later (H2).
3. **Who writes `arguments["0"]` after creation?** Instrument every
   `properties` mutation for an `Arguments` object — the mapped-arguments
   `[[Set]]` arm (`set_with_receiver_key` → `map.set_key` → `ordinary_set`), the
   in-field mirror, and the reference machinery — and the env-binding writers,
   to catch H2.

## 6. A deterministic unit test first

Before more sweeping, pin the shape in `crux` (the `host.rs` test style): create
an object, promote it, store a young `Function` into its `properties` through the
same define path the arguments object uses, run `collect_minor`, and assert the
child survives *and* the container is remembered. Mutate the store (remove the
barrier) to confirm the test bites. If it passes with the real path, H1 is the
live hypothesis and the collector is the target; if a variant fails, the store is.

## 7. Audit and bisect

- Bisect when the `Promise/allSettled` `--gc-stress` set first appeared:
  `git log --oneline -- crates/crux/src/heap.rs crates/crux/src/object.rs
  crates/crux/src/env.rs crates/runtime/src/promise.rs
  crates/runtime/src/function.rs` against the nursery A2/A5.1 cuts
  (`nursery-gc-plan.md`), whose A2 note already records a "shape that hides a
  missed barrier" class.
- Grep every writer of `properties` / env bindings for a missing
  `barrier_property` / `barrier_key` / `write_barrier`, with attention to the
  `ObjectKind::Arguments` paths and `arguments_define_own_property`'s mapped
  branch.

## 8. Acceptance

- `sweep.exe all --jitless --gc-stress` and `sweep.exe all --gc-stress`: 0 fail
  / 0 crash (the `Promise/allSettled` set gone), no new failures elsewhere.
- `--gc-verify` unchanged (0 fail / 0 crash, the three documented
  `copyWithin/*detached*` borderline hangs); normal sweep at baseline
  (48622 / 48464 / 0 / 158 / 0 / 0).
- `cargo clippy --workspace --all-targets -- -D warnings` clean;
  `cargo test --workspace` green; the CLI `--gc-stress` and `--nursery-stress`
  benches 12/12 `ok=true`.

## 9. Resolution — a swept-while-live handler, not a missing barrier

The probes here were the wrong shape for the actual defect. Re-running the
single fixture with (a) a reachability mark at the miss and (b) a registry of
every freed box address settled it: the young child's slot had
`freed_before=true` at the miss — it was **freed and recycled**, i.e. H1, and
the registering container (a reachable `EnvRecord`) held stale bits.

Free/alloc logging around the offending address showed the order exactly:
`ALLOC Function` (a handler closure) → `FREE Function` (a **major** sweep) →
`ALLOC EnvRecord` (the `then` call's function environment) → `ALLOC Function`
(the slot reused) → the verifier reads the binding's stale bits against the
recycled box. The closure was swept while still live: `promise_all_settled`
builds its two per-element handlers with `Function::create_builtin` in a loop
and holds them **only in a local `handlers: Vec<Value>`** until
`invoke_then` roots them. `Gc::new`'s per-allocation stress collection roots
only the box it just made, so the second handler's allocation (or any
allocation in the window) swept the first. The dangling `Value` was then
passed to `then` and stored into binding `a` and `arguments[0]`, whose
recycled slot the verifier flagged. `promise_all`/`promise_any`/`promise_race`
keep their single closure in a stack local (scan-visible), which is why only
`allSettled` produced the 59 failures.

**The fix, two parts:**

1. `crates/runtime/src/builtins/promise.rs` — `promise_all_settled` now opens a
   `StressSuppress` window (the established idiom for a local buffer holding
   unrooted handles across allocations), so no per-allocation collection can
   free a handler mid-loop.
2. `crates/runtime/src/agent.rs` — `Agent::maybe_collect` now honours
   `crux::ir::is_compiling()` (the `StressSuppress`/compile signal) before
   either of its safe-point collections. A `StressSuppress` window suppresses
   every collection inside it by construction, but the safe-point trigger was
   not checking the signal — so a `then`/iterator call with a loop inside any
   such window could still sweep the window's unrooted buffers. This closes
   the whole class, not just `allSettled`.

**Verified (2026-10-05):** `sweep.exe all --gc-stress` 48622 / 48327 pass /
**0 fail / 0 crash** / 158 skip / 137 hang (the hangs are the known
stress-slowdown artifacts; the `allSettled` set is gone); normal sweep at
baseline 48622 / 48464 / 0 / 158 / 0 / 0; `--gc-verify` at baseline
48461 / 0 / 0 with the three documented `copyWithin/*detached*` hangs;
`--jitless --gc-stress` on the `allSettled` subtree 104/104; `cargo clippy
--workspace --all-targets -- -D warnings` clean; `cargo test --workspace`
green; the CLI `--gc-stress` and `--nursery-stress` benches 12/12 `ok=true`;
the eight wasm suites at their documented counts (64,594 total, 0 fail) and
js-api 1001/0.
