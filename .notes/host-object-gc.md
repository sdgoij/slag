# Host objects and L2: traced host state, finalizers, weak handles

**Status (2026-09-25): slices 1-3 are in the tree; slice 4 is not.** Slice 1 — the
`ffi` handle-table rooting, §4.4 and the urgent part — landed with this note's own
regression tests (`ffi`'s `a_retained_value_survives_a_collection` and
`a_retained_string_survives_a_collection`, `crux`'s
`a_registered_root_source_keeps_its_boxes_alive`), and `.notes/embedding.md` §7 has
its record and §9 its bullet. **Slices 2-3 — the traced host objects and the
deferred finalizers, §4.1 and §4.2 — landed next**, with the tests §5's
criteria name: `crux`'s `a_host_objects_retained_edge_roots_its_value`,
`a_retained_edge_on_an_old_host_object_is_remembered`,
`a_swept_host_object_captures_its_finalizer` and
`a_host_object_value_cycle_collects_and_finalizes`, and `runtime`'s
`a_swept_host_object_finalizes_through_the_isolate`. `.notes/embedding.md` §7 has
that record, and §5's L2 row and §4's second acceptance test now read as landed
rather than aspirational.

**Slice 4 — weak persistent handles — is not started**, and §4.3 is its design.

Two of §4.2's decisions changed while it was implemented, both narrowed rather
than widened, and both are recorded here because the *reason* is part of the
design: the finalize capture rides the vtable's **`drop` slot** rather than a new
vtable slot plus a call site in each of the two sweeps (one funnel instead of two
places to forget, which makes §6's "there are two sweep paths" trap structurally
impossible), and no monotonic "already finalized" flag is needed, because a
payload is dropped exactly once — both sweeps clear the live bit before the drop
and every walk filters on it. The measurements are in `.notes/embedding.md` §7.

What is below is the design: the problem (§1), what L2 has to provide (§2), the
options (§3), the design of the chosen one (§4), the acceptance criteria (§5) and
the slices (§6).

L2 is the ladder level where "the host owns objects JS retains, and those objects
reference JS values" (`.notes/embedding.md` §5).

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
   a forced collection (fails today). Same for `HostOps`-held values. **Landed** —
   slice 1 wrote the `ffi` half first and measured the defect through it
   (`.notes/embedding.md` §7's record has the numbers), and slices 2-3 are the
   `HostOps`-held half.
2. A host object's retained value survives while the host object is reachable,
   and becomes collectable once the host object is swept. **Landed** —
   `crux`'s `a_host_objects_retained_edge_roots_its_value`.
3. `finalize` runs exactly once, after the collection, for a swept host box —
   including when the host still holds its `Rc`. **Landed** — `crux`'s
   `a_swept_host_object_captures_its_finalizer` (the queued behaviour *is* the
   host's `Rc`, so it outlives the box) and `runtime`'s
   `a_swept_host_object_finalizes_through_the_isolate`.
4. A host-object ↔ JS-value cycle collects fully (the case that never finalized
   before). **Landed** — `crux`'s `a_host_object_value_cycle_collects_and_finalizes`:
   the host object retains the object that holds it, and one precise collection
   takes both boxes and queues the finalizer the reference-count model could never
   fire.
5. `--gc-stress` / per-allocation variants of 2-4. **Partly landed, and the part
   that is missing is named.** `crux`'s
   `a_retained_edge_survives_a_collection_per_allocation` is 2 with a collection
   after *every* allocation: 32 rounds in which the host object is promoted first,
   so each round is an old box gaining a young edge, the barrier's store read out
   of the remembered set, a young unretained peer swept, and every value retained
   so far still live. It is precise because its entry point is the deterministic
   `Heap::collect_minor`. `runtime`'s
   `a_host_object_swept_under_gc_stress_finalizes_once` is 3 with the engine's own
   `--gc-stress` on, which also turns the barrier and minor verifiers on, and with
   **no host-driven collection in it**: the sweep that takes the object is the
   engine's. Not covered: 4 under stress, and the shape where a host's `finalize`
   itself runs host code inside the stressed window — the second belongs with the
   first host that does it.

   **The trap this cost a day to find, and it is about the *scan's window*, not
   about roots.** The conservative scan reads the words of `[sp, high)` — from the
   collecting frame's stack pointer *upward*, which includes returned callees'
   frames whenever the collection is called from deep enough below the frame that
   held the address. So "create the object in an id-only helper and keep nothing"
   is *not* enough: it makes the object unreachable for a collection called
   straight from a test frame and *reachable* for one running inside an eval, which
   is a difference no test should rest on. Measured, four arms of one experiment
   (a 256-allocation script, an id-only helper, with and without a stack scrub,
   with and without `--gc-stress`): without the scrub 0 finalizers in both stress
   modes — including with stress *off*, which is what ruled out stress mode as the
   cause — and with the scrub 2 in both, the arm's own object *and* the previous
   arm's (which the earlier arm had left alive). The remedy is `runtime`'s
   `scrub_stack`: write junk over the region the helper used, and the answer stops
   depending on where the collection was called from. Any future test that needs an
   object genuinely unreachable should use it, and should not conclude from a
   failing collection that the collector is wrong before checking the window.
6. A weak handle fires once, after the collection; the target's slot is reclaimed;
   the callback cannot resurrect it. **Slice 4, not started.**

## 6. Slices

1. **ffi handle-table rooting** + its regression test (fixes a false promise).
2. **`HostOps::finalize`** + the deferred queue + `run_finalizers`.
3. **Engine-owned host edges** (`host_object_retain/release`, `ObjectKind::trace`
   visiting them, barrier on retain).
4. **`Weak` handles** on the existing ephemeron/weak machinery.
5. Later, optional: cppgc-shaped names (`Traced`/`Member`/`Visitor`/
   `initialize_process`) as sugar over 3-4.

### Slices 2-3 — landed

*(Slice 1's own account follows below; it was the first of the three.)*

**Edges.** `ObjectKind::Host` carries a `HostObject` — the `Rc<dyn HostOps>`
behaviour it always carried, plus a `RefCell<Vec<Value>>` edge list — and
`Trace for ObjectKind` visits it, so a retained value is marked *through* the host
object. `JsObject::host_object_retain` runs `write_barrier` (it is a store: an old
host object gaining a young edge must be remembered or a minor sweeps the child);
`host_object_release` removes one retention and needs no barrier. Retain/release
pair up; releasing a value that was never retained is a no-op. Keeping the
behaviour shared and the list per object is what left the one call site that
compares a host class by address (`crates/jsc/src/value.rs`) a one-line change.

**Finalization.** `Trace::request_finalize` is a trait hook (default no-op) that
the vtable's `drop` slot calls **before** `drop_in_place`; `impl Trace for JsObject`
overrides it to capture `(behaviour, object_id)` into
`crux::host::PENDING_FINALIZERS` while the behaviour handle and the identity are
still in the payload. `HostOps::finalize(&self, object_id)` is the callback;
`crux::host::take_pending_finalizers` drains; `Isolate::run_finalizers()` runs
them, at the top of every job drain and at an outermost `Context` entry, so a host
that never asks still gets its finalizers.

**The trap, and why it is not one here: there are two sweep paths.** The major in
`collect_from_work` and the minor in `collect_minor_inner` each have their own drop
loop, so hooking only one would let the finalizer run in one collection mode and
not the other. The capture rides `GcBox::<T>::VTABLE.drop` — the one slot both
loops call — which is why it cannot be forgotten in either. (This is the change
from the design as first written, which had a second vtable slot and a call site in
each sweep.)

Finalization has to be deferred rather than run in the sweep, because the sweep is
mid-collection with the heap borrowed and a finalizer may allocate or run JS —
and `--gc-stress` collects per allocation, so re-entering would be immediate. That
is also why no monotonic "already finalized" flag is needed: a box's payload is
dropped exactly once, because both sweeps clear the live bit before the drop and
every walk filters on it. And nothing runs finalizers at isolate teardown, because
teardown drops no payloads — a host that never triggers a collection gets none,
which is the same statement as "a finalizer is the collector's decision".

**Coverage it has.** `crux`'s `a_host_objects_retained_edge_roots_its_value`
(the deterministic no-scan `Heap::collect`: the retained value survives the
collection that sweeps an equal, unretained peer, and releasing the edge puts it
back), `a_retained_edge_on_an_old_host_object_is_remembered` (the barrier, read out
of the remembered set, with the object deliberately not passed as a root to the
minor that follows), `a_swept_host_object_captures_its_finalizer` and
`a_host_object_value_cycle_collects_and_finalizes` (the case §1(b) says the old
timing could never reach); `runtime`'s `a_swept_host_object_finalizes_through_the_isolate` (end-to-end through
`Isolate::run_finalizers`). The masking trap the design named is handled in the
last one by creating the object in an `#[inline(never)]` call whose frame is
returned before the collection, so no stack word roots it — and by asserting the
*exact* list of identities, which makes a survivor fail as loudly as a
non-finalizer. The other named trap is gone: the collection is the engine's own
`Agent::collect_garbage`, not an eval that may or may not sweep.

`jsc` keeps its release-driven finalize timing for now: it is the retired C
surface and its compat test asserts the old behaviour, so wiring it to
`HostOps::finalize` is a deliberate separate change.

### Remaining — slice 4: weak persistent handles

`Global::set_weak`-shaped weak handles with a GC callback, built on the existing
weak machinery (`Agent::weak_ref_targets`, `heap::note_ephemeron`) and the GC-4
rule that the dead set comes from the *precise* mark. Not started.

### Slice 1 — landed

The slice registers the `ffi` tables as a root source so a value a host holds
through them is marked rather than swept. `crux::heap` gained the registry —
`RootSource`, `register_root_source` (idempotent by address), consulted by
`pinned_roots` beside the pins, which all three collection entry points already go
through — and `ffi::tables` registers one `'static` source visiting both tables
through the `Trace` each entry implements, at the first retention on a thread. A
source reads a table and cannot call back into the registry, which is why the
registry borrow is held across the visit; a table's own borrow cannot be live
either, because nothing in `insert`/`get`/`remove` allocates in the arena and so
nothing can collect inside them.

**The defect it demonstrates, measured before the fix.** Removing the registration
*is* the mutation check: with it removed, `a_retained_value_survives_a_collection`
reports `a value the host still holds was swept: [2814056702016]` and the string
test reports its rope's three part boxes — the aliasing §1(a) predicted, and the
reason this note writes the test first. The mechanism itself is covered precisely
(with the collector's own swept set) in `crux`'s
`a_registered_root_source_keeps_its_boxes_alive`; the `ffi` tests are the
end-to-end ones.

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
