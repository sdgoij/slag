# Host objects and L2: traced host state, finalizers, weak handles

Design decision, approved and largely landed: **slices 1-3 have shipped** (see
§6); slice 4 (weak handles) is not started. L2 is the ladder level where
"the host owns objects JS retains, and those objects reference JS values"
(`.notes/embedding.md` §3). Nothing below is implemented; this note is the
proposal plus the evidence it rests on.

## 1. The problem

Three separate things are missing, and the first is a defect, not a gap.

**(a) Host-held JS values are not roots.** `ObjectKind::Host(Rc<dyn HostOps>)`
(`crates/crux/src/object.rs:464-471`) exists to dispatch *behaviour*
(`get`/`set`/`has`/`delete`/`own_property_keys`/`call`/`construct`). Its `Trace`
impl deliberately contributes no edges — "Ordinary, IsHTMLDDA, External, and
Host (deliberately Rc) carry no GC heap edges" (`crates/crux/src/object.rs:750-762`)
— and `HostOps` (`crates/crux/src/host.rs:19-111`) has no `trace` and no
`finalize`. So a `Value` a host object holds is invisible to the collector: it
gets swept, its arena slot is handed to the next allocation, and the host's
handle **aliases** whatever lands there. Silent, not a crash.

The `Host` variant's doc claims the opposite — "host state is not GC-managed; the
ffi/jsc tables root it, GC-6" (`object.rs:466-470`). GC-6 is a *verification*
cut in the plan ("Confirm … the `jsc`/`ffi` handle tables root host-held refs
correctly", `gc-plan.md:476-481`), not a implemented mechanism. Reading the code,
nothing registers either table with the collector: they are `thread_local!`
`HashMap`s of `Value`/`JsString` (`crates/ffi/src/tables.rs:53-58`) with no
`Trace` impl and no registration API, which is exactly the "native heap buffer
the stack scan cannot see" case that `gc-plan.md`'s GC-7 section documents as a
real root gap (it was found the hard way on wasm). So the C surface's promise —
"values handed to the host never dangle" (`crates/jsc/src/lib.rs:13-16`) — holds
only until the first collection. *Inferred from the code, not yet demonstrated
by a test; slice 1 below writes that test first.*

**(b) Finalization is not GC-decided.** `crates/jsc/src/class.rs` registers a
`finalize` callback per object id and invokes it when the ref table releases the
last reference (`invoke_finalize`, `class.rs:395-400`), which is why the crate
doc admits "`finalize` runs when the object's last strong reference drops, so
cyclic JS graphs never finalize" (`crates/jsc/src/lib.rs:21-23`). Host object
lifetime is therefore not the collector's decision, and a host-object/JS cycle
never finalizes.

**(c) There is no weak persistent handle.** V8's `Global::SetWeak` (a callback
when the only remaining references are weak) has no analogue; `Global` is strong
only (`crates/runtime/src/api/handle.rs`).

## 2. What L2 has to provide

1. **Host edges participate in marking** — a JS value the host retains is reachable.
2. **Host finalizers run when the collector decides the object is unreachable**,
   exactly once, outside the collection.
3. **Weak handles** notify the host when a target dies, without resurrecting it.

## 3. Options

**A. Traced host objects in the one arena.** Keep `ObjectKind::Host` as the
storage; add an engine-owned edge list the collector traces (plus an optional
Rust `trace` hook), a deferred finalizer queue, and weak handles on the existing
weak/ephemeron machinery.

**B. A cppgc-equivalent second heap.** `Heap`/`Visitor`/`Traced`/`Member`/
`GarbageCollected`/`initialize_process`, cross-heap marking.

**C. No GC participation.** Hosts pin what they retain (`Global`/`Pin`, L1) and
release in their own `Drop`; the engine stays out of it.

| Criterion | A | B | C |
|---|---|---|---|
| Cycles (host ↔ JS) | correct | correct | leak forever |
| Usable from a C host | yes (engine-owned edges) | yes, but the host must implement `GarbageCollected` | host must manage pins by hand |
| Usable from a Rust host | yes | yes | yes |
| Engine cost | edge list + finalize queue + weak table | a second collector and a cross-heap mark | zero |
| Cycles among *host* objects | N/A (host's own concern) | handled | N/A |
| V8/Node-API porting fidelity | shape added later as sugar | high (names match) | none |
| Testability | high (single heap, existing stress modes) | high but larger | low (bugs are silent leaks) |

**Decision: A**, with **C documented as today's interim** (hosts must pin), and
B's *names* layered later if a Deno/Blink-shaped host wants them. The reason B is
not chosen now: cppgc exists because Blink's Oilpan objects live in their own
heap with their own allocation policy and lifetime domain, so V8 needs a
cross-heap mark. Slag's host objects **already live in the JS arena** as
`ObjectKind::Host` boxes, so there is no second lifetime domain to reconcile —
only missing edges. A second heap would buy nothing and cost a second collector.

## 4. Design of A

### 4.1 Edges: an engine-owned list, barrier-correct

```rust
// crux::object
JsObject::host_object_retain(host: &Handle<JsObject>, value: Value)
JsObject::host_object_release(host: &Handle<JsObject>, value: Value)
```

The engine stores the retained values on the host object and traces them from
`ObjectKind::trace`, so `ObjectKind::Host` becomes a traced kind. `retain` is a
store and therefore runs the barrier (`write_barrier_element`, `heap.rs:1107`)
so a minor collection sees an old host box's new young edge — an in-place edge
list the engine owns is the only mechanism that can do this for a C host.

For Rust hosts that keep values in their own structures, `HostOps` gains an
optional oracle:

```rust
fn trace(&self, _visit: &mut dyn FnMut(GcAny)) {}   // default: no edges
```

with the obligation documented: values reported here must be stable between
collections *or* reported through `host_object_retain` when they change on an
old box, because the hook cannot carry a barrier. Treat it as an optimization,
not the contract.

### 4.2 Finalizers: deferred, exactly once

```rust
fn finalize(&self) {}   // HostOps, default no-op
```

Called when the collector sweeps the host box — *not* when an `Rc` drops. The
sweep cannot run host code (`finalize` may allocate and run JS, and even without
that we are mid-sweep with the heap borrowed), so it enqueues and the queue is
drained afterwards. The pattern already exists: the GC-4 leak fix routes
FinalizationRegistry cleanups through `Agent::pending_cleanup_jobs`, "enqueued by
the collector's compaction hook (it cannot touch the job queues — the collector
runs with `&self`)" (`crates/runtime/src/agent.rs:875-881`), promoted at the next
drain. Host finalizers ride the same shape, with two additions: a monotonic
"finalized" flag per host object so a second collection cannot re-run it, and an
`Isolate`-level entry point to drain them deterministically (a test needs that;
V8 lets the embedder drive it via the platform).

An `Rc` the host still holds after the box was swept stays a live Rust value
whose JS wrapper is gone: legal, and the reason `finalize` is a separate callback
rather than `Drop`.

### 4.3 Weak persistent handles

`Global::set_weak(callback)` / a `Weak` handle type, built on the existing weak
machinery rather than a new table: the ephemeron fixpoint (`heap.rs`'s
`EPHEMERONS`, `note_ephemeron`) and `Agent::weak_ref_targets` already model
"target dead → act after collection", including the GC-4 rule that the dead set
comes from the **precise** mark so a stale stack word cannot keep a target alive.
Callbacks fire once, after the collection, and must not resurrect the target.

### 4.4 The C-surface fix (slice 1, the urgent part)

The `ffi` handle tables must become a traced root source. Cleanest given the
thread-local design: a per-thread **handle-owner registry** the collector
consults, with `ffi::tables` registering its two tables at first use. A worker
thread's tables then root only that thread's heap, which matches the existing
per-thread heaps and `workers` storage.

## 5. Test plan (the acceptance criteria)

Every test forces a collection with the scan's blind spot (a heap-buffer or boxed
handle), the way the L1 pin tests do, or the conservative scan will mask the bug.

1. **The defect, first:** a `Value` retained only through the `ffi` table survives
   a forced collection (fails today). Same for `HostOps`-held values.
2. A host object's retained value survives while the host object is reachable,
   and becomes collectable once the host object is swept.
3. `finalize` runs exactly once, after the collection, for a swept host box —
   including when the host still holds its `Rc`.
4. A host-object ↔ JS-value cycle collects fully (the case that never finalized
   before).
5. `--gc-stress` / per-allocation variants of 2-4 (`StressSuppress` where host
   code runs inside a collection window).
6. A weak handle fires once, after the collection; the target's slot is reclaimed;
   the callback cannot resurrect it.

## 6. Slices

1. **ffi handle-table rooting** + its regression test (fixes a false promise).
2. **`HostOps::finalize`** + the deferred queue + `run_finalizers`.
3. **Engine-owned host edges** (`host_object_retain/release`, `ObjectKind::trace`
   visiting them, barrier on retain).
4. **`Weak` handles** on the existing ephemeron/weak machinery.
5. Later, optional: cppgc-shaped names (`Traced`/`Member`/`Visitor`/
   `initialize_process`) as sugar over 3-4.

### Slices 2-3 — landed: edges and finalization

*(Slice 1's own account follows below; it shipped first.)*

**Edges.** `ObjectKind::Host` now carries a `HostObject` — the behaviour plus an
`Rc<RefCell<Vec<Value>>>` edge list — and `Trace for ObjectKind` visits it, so a
retained value is marked *through* the host object. `JsObject::host_object_retain`
runs `write_barrier` (it is a store: an old host object gaining a young edge must
be remembered or a minor sweeps the child); `host_object_release` removes one
retention and needs no barrier. Retain/release pair up; releasing a value that
was never retained is a no-op.

**Finalization.** `Trace::request_finalize` is a new trait hook (default no-op)
with a matching vtable slot; `impl Trace for JsObject` overrides it to capture
`(ops, object_id)` into `crux::host::PENDING_FINALIZERS` — *before* the payload is
dropped, because the behaviour handle and the identity live in it.
`HostOps::finalize(&self, object_id)` is the callback;
`crux::host::take_pending_finalizers` drains; `Isolate::run_finalizers()` runs
them; and `Context::with_call` calls it at depth 0, so a host that never asks
still gets its finalizers after every outermost call.

**The trap worth remembering: there are two sweep paths.** The major in
`collect_from_work` and the minor in `collect_minor_inner` each have their own
drop loop. The first cut hooked only the major, and the runtime test caught it —
the finalizer never ran under `--gc-stress`. Both call the vtable hook now.
Anything the sweep must do per dead box has to be added in both places, or it
silently works in one collection mode and not the other.

Finalization is deferred rather than run in the sweep because the sweep is
mid-collection with the heap borrowed and a finalizer may allocate or run JS —
and `--gc-stress` collects per allocation, so re-entering would be immediate.

**Coverage.** `crux`'s `a_host_objects_retained_edge_roots_its_value` (precise:
the collector's own liveness flag, deterministic `collect(&[])`) and
`a_swept_host_object_captures_its_finalizer`; `runtime`'s
`a_swept_host_object_finalizes_through_the_isolate` (end-to-end through
`Isolate::run_finalizers`). Two masking traps surfaced while writing that last
test, and both are now designed around: the object must be created *and released
inside a returned frame* (otherwise stale stack words root it), and a trivial
`({})` eval does not necessarily trigger a *sweeping* collection — nursery
stress plus a 200-iteration allocation loop does.

`jsc` keeps its release-driven finalize timing for now: it is the retired C
surface and its compat test asserts the old behaviour, so wiring it to
`HostOps::finalize` is a deliberate separate change.

### Remaining — slice 4: weak persistent handles

`Global::set_weak`-shaped weak handles with a GC callback, built on the existing
weak machinery (`Agent::weak_ref_targets`, `heap::note_ephemeron`) and the GC-4
rule that the dead set comes from the *precise* mark. Not started.

### Slice 1 — landed, with the defect demonstrated

The `ffi` tables are now registered as a root source: `crux::heap` gained
`register_root_source`/`registered_root_source_count` (a per-thread list of
`fn(&mut dyn FnMut(GcAny))`), the collector's three entry points visit it through
`with_engine_roots` alongside the L1 pins, and `ffi::tables::retain_value` /
`retain_string` register once per thread. Both tables are traced, because a bare
`JsString` can hold arena handles (`ConsString`/`Rope`/`Sliced` — `impl Trace for
JsString`), so a retained `JSStringRef` to a rope was equally exposed.

**The defect is now demonstrated, not inferred.** With the registration removed
(a mutation check), `a_retained_value_survives_a_collection` fails with the
aliasing exactly as predicted: the retained ref resolves to the *churn* object
that reused the swept slot (`left: 2, right: 1` — the retained object's id was 1).
With the registration in place the ref keeps its identity across a collection.

The mechanism itself is covered precisely (with the collector's own liveness
flag) in `crux`'s `a_registered_root_source_keeps_its_boxes_alive`; the `ffi`
test is the end-to-end one and shares the weaker slot-reuse detector with the L1
notes rather than the flag.

Re-certified after the change (`crux::heap` is in the sweep graph, so unlike the
façade work this required a re-sweep): test262 48,464 + 3,205 = **51,669 pass of
51,979, 0 fail / 0 crash / 0 hang**, wasm **64,594 checks** and **1,001 JS-API
tests**, all 0 fail — every number identical to the pre-change baseline.

## 7. Traps and risks

- **No host code inside a collection.** Finalizers and weak callbacks are
  enqueued, never called from the sweep; `--gc-stress` collects on every
  allocation, so a finalizer that allocates is a re-entrancy hazard to test for.
- **The barrier is load-bearing.** A late edge on an old host box without
  `host_object_retain` is a minor-collection bug; the `verify_barrier`/
  `--gc-verify` machinery (`heap.rs`) is the detector.
- **The scan hides bugs.** Anything held in a `Vec`/`Box` is invisible to the
  stack scan; a test that keeps its handle in a local will pass whether or not
  the fix is correct.
- **Finalizer ordering vs FinalizationRegistry** is unspecified until we pick:
  propose host finalizers first (they are the host's own bookkeeping), then
  cleanup jobs, both in the same post-collection drain.
- **`Value` stays `!Send`** (`crux/src/value.rs:90-91`); the registry must stay
  per-thread rather than global.

## 8. Open questions for review

1. **Edge mechanism:** engine-owned list only, or also the Rust `trace` hook?
   (Proposal: list is the contract, hook is a documented optimization.)
2. **Who drains finalizers:** the next job drain only, or also an explicit
   `Isolate::run_finalizers()`? (Proposal: both; tests use the explicit one.)
3. **Adopt cppgc names now or later?** (Proposal: later — shape is sugar, and
   the note's §3 argues the second heap is not needed.)
4. **Is `Weak` (item 4) in this cut, or the next?** It is the only L2 item a
   Deno-class host in a browser-like role needs less than Node-API does.
