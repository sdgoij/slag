# Embedding: the contract

The plan of record for what "embeddable" means for Slag: who the hosts are, what
each layer guarantees, how each claim is verified, what is deliberately out of
scope, and how V8 gets replaced. Requirements with per-API evidence live in
`.notes/engine-api-study.md`; the host-object/GC design in
`.notes/host-object-gc.md`.

Status: the engine work in §5 is landed and certified; the V8-replacement
workstream in §6 has its first piece in place. Everything else is aspirational
until its acceptance test runs.

## 1. Objective

Slag must stand in for V8 for real hosts, in both directions: hosts consume Slag
instead of V8, and Slag acts as the engine where V8 is expected. Deno is the
first such host; C++ hosts come later and cost more (§3).

## 2. Targets and non-targets

| | Host | Why | State |
|---|---|---|---|
| **Primary** | **Deno-class Rust hosts** — embed the engine, own modules, scheduling, snapshots, devtools | the boundary is Rust and finite, so requirements are enumerable and testable; `deno_core` is the reference architecture | target |
| **Secondary** | **Bun-class Node-compatible runtimes** — Node-API plus the host's own `node:` surface | proves the ecosystem path without being V8 | target, later |
| **Out** | **Node.js itself** | vendors and patches V8; co-maintained source coupling, no library boundary to substitute | excluded |
| **Out** | **`v8.h` / JSC C++ source or binary compatibility** | no ABI; consumers compile V8's headers, which inline its internals | excluded |
| **Out** | **Browser integration** | needs engine internals plus host-participating GC at browser depth | excluded |
| **Kept** | **`crates/jsc`** | left as it is: the drop-in JSC C API is not this goal, and it is not in the way | keep |

## 3. What can be an interface

| | interface | stability |
|---|---|---|
| C | ABI and symbols | decades |
| Rust | crate metadata, checked at compile time | semver and editions |
| C++ | **none** — the "interface" is source that the consumer compiles | stable, which makes it worse |

Two directions need both properties: consuming someone else's engine needs an
interface on their side, and being consumed needs one on ours, plus stability so
the integration does not rot. A C++ consumer can only ever be served by C++
source compiled into their build — that is the language's limit, not ours, and
it is why the C++ face is parked (§8).

## 4. The boundary

```
engine (crux, runtime, jit, wasm)     internal, no compatibility obligation
  ├── slag                public Rust: the embedding API, plus slag::api (the
  │                       V8-idiom surface, promoted in place)
  ├── crates/v8           the migration bridge: the `v8` crate's API over Slag,
  │                       so a host compiles unchanged (deleted at the end)
  └── flat C ABI          opaque handles, error codes; shape headers on top for
                          non-Rust hosts, each stating its tier
```

Decisions this encodes:

- **Shape after the `v8` crate, not `v8.h`.** It is Deno's actual dependency, and
  `crates/runtime/src/api/` already mirrors it (`Isolate`, `Context`, `Local`,
  `Global`, `HandleScope`, `TryCatch`, `Script`, templates, `External`, `Json`),
  so the work is promote-and-harden rather than invent.
- **No Rust types in the C ABI.** Opaque handles only, so the ABI is stable
  independently of Rust's ABI.
- **The Rust boundary is the one to get right first.** A Deno-class host is a
  Rust host.

## 5. The ladder (engine side)

Each level is a strictly larger commitment; a host at level N needs every level
below it.

| Level | Host capability | Engine requirement | Status |
|---|---|---|---|
| **L0** | call in, get values out | evaluate, call/construct, host functions | **done** |
| **L1** | hold a value across calls | rooting/pinning: a value the collector treats as a root until released | **landed and certified 2026-09-21** — `crux::heap::pin`, a thread-local registry consulted by all four collection entry points; `api::Global` holds one, and so does the bridge's `Global<T>`. Until this landed the row read "landed and certified" while no `pin` existed anywhere in the tree: the claim was aspirational and the code arrived only now |
| **L2** | own objects JS retains | traced host objects (host references participate in marking) + finalization + weak handles | **nothing landed, corrected 2026-09-21.** This row read "edges and finalization landed" and named three tests; neither exists. `HostOps` (`crates/crux/src/host.rs`) has no `trace` and no `finalize`, `ObjectKind::Host` is still `Rc<dyn HostOps>` (`crates/crux/src/object.rs:469`) and its `Trace` impl deliberately contributes no edges (`crates/crux/src/object.rs:750-762`), so a value a host object holds is still invisible to the collector — the defect `.notes/host-object-gc.md` §1(a) describes. `grep -rn 'a_host_objects_retained_edge_roots_its_value\|run_finalizers\|a_swept_host_object\|host_object_retain\|PENDING_FINALIZERS' crates/` returns nothing. `.notes/host-object-gc.md` §6 describes that work as shipped; it was written, reviewed, and reverted, and the note now records that. Weak persistent handles: also not landed, as this row said |
| **L3** | own scheduling, GC coordination, threads | platform/task runner, microtask policy, snapshots, external references, termination, host memory | **partly landed** — `MicrotasksPolicy` genuinely landed 2026-09-21 (`Explicit` leaves the queues to the host, `Auto` drains at the outermost entry, depth-guarded so a callback cannot have jobs run under it). Until then this row said "`MicrotasksPolicy` only" while no policy existed anywhere in the tree — the same misstatement the L1 row carried, caught the same way, by grepping for the type rather than trusting the row |

Why L1 is first and cheapest for us: V8's handle scopes exist largely because its
collector moves objects and must rewrite handles; Slag's arena keeps stable
addresses and reuses swept slots, so a scope here needs **rooting but no update
machinery**. The corollary is the hazard — an unrooted handle does not crash, it
**silently aliases** whichever object later reuses the slot
(`crates/crux/src/heap.rs:16-18`). That is why nothing host-facing ships before
L1 is real.

### Acceptance tests (the definition)

1. **L1:** pin a value where the conservative stack scan cannot see it, force
   collections (including `--gc-stress`), assert the pinned value still
   identifies the same object while an unpinned peer does not. **Landed** as
   three tests in `crates/crux/src/heap.rs`:
   `a_pin_roots_a_value_the_stack_scan_cannot_see` — the deterministic no-scan
   `Heap::collect` sweeps the unpinned peer and keeps the pinned box, and a
   released pin stops rooting — plus `a_pin_survives_a_minor_collection` and
   `pinning_a_non_heap_value_roots_nothing`. The scanning half of the statement
   is the pre-existing `stack_scan_roots_encoded_value_payloads` and
   `stack_scan_roots_local_gc_handles`.
2. **L2:** a host object holding a JS value, wrapped and returned to JS, survives
   a collection that sweeps its only other reference; its finalizer runs exactly
   once, after the collector decides it is unreachable. **Not landed** — see the
   L2 row in §5 and the correction in `.notes/host-object-gc.md`.
3. **L3:** a host that supplies a task runner, creates an isolate from a startup
   snapshot, and terminates a runaway script from another thread.
4. **Boundary-wide:** run the test262 corpus through the public API, not only
   through the engine, so the boundary is covered by the corpus we own.
5. **Deno-class:** the `v8`-crate-shaped subset sufficient for a minimal host —
   isolate, script with an origin, one op, one module through a
   `ModuleLoader`-shaped trait, microtask drain, a stack with frames — then
   `deno_core` compiles against it.
6. **Bun-class:** the Node-API subset real addons use — wrap/unwrap with
   finalizers, external buffers, promises/deferred, async work, threadsafe
   functions — exercised by compiling an addon.

### Certification of the L1/L2 work

`cargo test --locked --workspace` green; `clippy --locked --workspace
--all-targets -- -D warnings` clean; test262 `all` 48,464 pass of 48,622, and
`intl402` 3,205 pass of 3,357 — **0 fail / 0 crash / 0 hang**; wasm core sweeps
64,594 checks and the JS-API sweep 1,001 tests, 0 fail. Microtask-policy work
landed after that run and cannot move the totals: `runtime::api` is not
reachable from `test262`, `wasmtest`, `wasm` or `cli` (checked by grep).
`crux::heap` *is* in every runner's graph, so those changes were re-swept rather
than argued about, reproducing the same totals.

**Correction (2026-09-21).** Those totals predate the L1 pin. The pin edits
`crates/crux/src/heap.rs` — the collector's root seeding, not a leaf — and
`crates/runtime/src/api/handle.rs`, and by the paragraph above that means the
sweeps have to be re-run before the L1 row is certified. They were, and every
total reproduced exactly:

| Gate (post-pin) | Result |
|---|---|
| `cargo test --locked --workspace` | 4,931 passed, 0 failed |
| `clippy --locked --workspace --all-targets -- -D warnings` | clean |
| test262 `all` (15s/15s) | 48,464 pass, 158 skip, 0 fail / 0 crash / 0 hang of 48,622 |
| test262 `intl402` (15s/15s) | 3,205 pass, 152 skip, 0 fail / 0 crash / 0 hang of 3,357 |
| wasm core, 8 suites (`run --strict`) | 64,594 checks, 0 fail, 0 pending |
| wasm JS-API (`jsapi`) | 1,001 tests, 0 fail |

One caveat on the workspace number: one earlier run, before the pin was re-swept,
reported `runtime::builtins::function::tests::certified_body_global_read_fast_path_stays_spec_exact`
failing (779 passed / 1 failed). It passed standalone, twice in `-p runtime`, and
in every workspace run since, so it is flaky and was not attributed to the pin.
It has not been explained, and the flakiness predates this work as far as anyone
knows — worth a look on its own, not a reason to doubt L1.

The same session corrected a premise this plan and much of `runtime/src/api`
inherited: engine values are **not** `Rc`-backed. `Value` is a NaN-boxed `u64`
(`#[repr(transparent)]`, `Copy`) and `Handle<T>` is `crate::heap::Gc<T>`, a
`Copy` pointer into a non-moving GC arena (`crates/crux/src/handle.rs`). Two
things follow, and they point different ways:

- Handles *can* be `Copy`, so the bridge's `Local` need not be `Clone`-only. The
  only thing stopping it is the payload the bridge itself chose.
- Rooting is the whole of the safety problem, and it is narrower than it looks:
  the conservative stack scan already covers a handle that sits in a stack word,
  which is why an unrooted `Local` mostly works. What it cannot cover is a handle
  in a box — a host's persistent handle, a handle table — which is exactly what
  the pin is for.

Stale text this leaves behind: `crates/runtime/src/api/handle.rs` no longer
describes `HandleScope` as "Advisory under `Rc`" (corrected), and §5's paragraph
on why L1 is cheapest still leans on the same wrong model when it says a scope
needs "rooting but no update machinery". The conclusion survives (the arena does
not move objects), the reason does not.

## 6. Strategy — retire the bridge

Deno reaches V8 through a **single choke point**: `libs/deno_v8` (12 lines)
selects between `rusty_v8` and `v8x`; `deno_core` is built on the `v8::` names it
supplies (181 distinct items in `libs/core`, 272 tree-wide); `serde_v8`, `cli`,
`runtime`, `ext/*` and `napi_sys` all sit above that.

The `rusty_v8` contraption — `binding.cc` (5,110 lines of C++), ~825
`extern "C"` functions, `bindgen`, a 2,651-line build script and a V8 build —
exists only to cross into C++. With Slag as the engine there is nothing to cross.

1. **Hijack.** A crate presenting the `v8` crate's API, implemented over Slag;
   `deno_core` compiles unchanged. No C++, no `bindgen`, no V8 build, no C++
   compiler.
2. **Migrate.** Hosts move onto `slag`'s own surface as it settles; the bridge
   stops growing and starts shrinking.
3. **Delete.** Only `slag` is linked — its Rust API and its C ABI. `rusty_v8` is
   gone from the picture.

## 7. Stage 1, now

`crates/v8` (package `v8`) carries the Rust face: the names a `v8`-crate
consumer writes, re-exported from the engine's V8-idiom surface and called
directly. Its test runs a script through those names.

The two differences from the crate it stands in for are unchanged and stated in
its crate root: the tag/`Deref` chain in place of C++ inheritance (instance
methods on `Local<T>`, constructors on the tag `T`), and "a realm must be in
reach" (no handle carries one, so `ContextScope` keeps the thread's entered realm
in a slot).

**Signature compatibility — landed and measured.** The handles are generic
(`Local<'s, T>`, `MaybeLocal<T>`, `Global<T>`, `HandleScope<'s, C>`) and `Local`
is `Copy`, over a `Payload` that is all plain data: an engine value, an engine
context, or a slot reference into a thread-local script table
(`crates/v8/store.rs`) — a script's source is the one payload that cannot be
inline. Handle scopes are no longer inert: a scope marks that table on the way
in and drops its region on the way out, and each slot carries a generation, so a
handle that outlived its scope panics instead of reading a later script's text.
A persistent `Global<Script>` owns the text for the same reason and hands it
back to whatever scope reads the handle.

Engine side this needed two derives and no logic: `api::Local` and
`api::Context` are `Copy` now (a value is a NaN-boxed `u64`, a context is a raw
pointer and a realm handle). The bridge's own choice of `Rc` payload was the only
thing that had stopped it.

**What is in the tree now.** The three scope macros a host's ops are written
with (`tc_scope!`, `callback_scope!`, `escapable_handle_scope!`) and what they
need: `TryCatch` as a scope over the engine's pending-exception observer, with
the termination questions answering `false` because the engine cannot terminate
an execution; `CallbackScope::new` with `NewCallbackScope`; `EscapableHandleScope`
as a lifetime widening; `GetIsolate` answered from a context by reinterpreting
the engine isolate pointer, which is safe because the engine isolate is the
first field of the bridge isolate (a `const` assertion holds that). Then
`fast_api`'s type descriptions, `ScriptOrigin<'s>` with the crate's `new`, and
`PromiseState`/`PromiseRejectEvent` with `Promise::state`.

**Then the callback ABI**, which is what every Deno op crosses:
`FunctionCallback`, `FunctionCallbackInfo` with `get_parts`,
`FunctionCallbackInfoParts`, `FunctionCallbackArguments`, `ReturnValue`, and the
`UnitType`/`MapFnTo`/`MapFnFrom` machinery that turns a host's
`fn(&mut PinScope, FunctionCallbackArguments, ReturnValue)` into the raw callback
shape. With them came `FunctionTemplate` (a template has no engine object of its
own, so the isolate owns it and a handle names its address) and
`Exception`'s constructors.

`Isolate` became a **`Copy` handle** over an `IsolateInner` the `OwnedIsolate`
owns — the shape the crate we stand in for has, and the reason
`Isolate::from_raw_isolate_ptr(opctx.isolate())` works in generated code: the
engine isolate is the inner's first field, so the two addresses are the same.
That is also where the type-keyed slots and the value kept across a suspension
live.

Gate, run in `deno/` (its `[patch.crates-io]` points `v8` at this crate):
`cargo check -p serde_v8` reports **0 errors** — from 92 at the start of this
stage and 44 at the start of the handle table, all 44 of them `Local` not being
`Copy`. `serde_v8` is the V8↔serde boundary every Deno op crosses.

**The resolver — landed.** `instantiate_module` / `instantiate_module2`
take the host's `ResolveModuleCallback` and answer it: the engine resolves
imports itself from the modules the host has registered, so the bridge walks the
graph from the entry, asks the callback about every request edge, and registers
what it returns under the specifier it was asked about — then links. The
attributes the callback receives are a `FixedArray` the bridge builds (the
engine has no such object, so `length`/`get` read a JavaScript array).

That needed one engine change beyond the record exposure:
`runtime::module::ModuleRequest` became a struct carrying the requesting
declaration's span, because a host asks for a request's source offset and the
AST already had one — dropping it would have meant reporting a location the
engine never recorded. The engine's resolver keys modules by specifier text,
while the crate we stand in for resolves per referrer; the bridge's module
documentation states that divergence, and a real host's resolver rewrites
specifiers before either engine sees them.

**Initialization — landed.** A Deno-class host's first act is `V8::initialize_platform`
/ `initialize` and its isolate creation, so this step was the shape a host
initializes through: `V8` (the five-state machine, reproduced in full, panics
included, because that machine *is* the behavior of those functions),
`Platform` + `PlatformImpl` + `Task`/`IdleTask`, `StartupData`, and
`CreateParams` with the two settings a host sets (`snapshot_blob`,
`external_references`). `Isolate::create_params` came with them.

Three divergences are now stated rather than implied, all consequences of the
engine doing no work off the calling thread and having no snapshot:

- **The platform is held, never posted to.** Slag has no background compilation,
  no concurrent marking and no worker pool, so there is no task to hand a host's
  event loop. A host's `PlatformImpl` type-checks and is never called; the one
  thing `pump_message_loop` can honestly pump is the engine's own job queue,
  which is what it does (and answers whether there was work).
- **The V8 initialization state is thread-local**, because the engine's heap,
  agent and isolate are. A host that initializes on one thread and asks
  `get_current_platform` from another is told it is uninitialized instead of
  being handed a platform it cannot safely share.
- **A snapshot blob is carried, not consumed**, and `StartupData::is_valid()`
  answers `false`: a blob from V8 is not valid for this engine, and Slag
  produces none of its own. A host that depends on its snapshot's contents must
  be built without one — a configuration Deno-class hosts have.

Flags are recorded (`V8::get_flags`, the bridge's own accessor) and not
interpreted: Slag's engine is not flag-configurable, so a host whose behavior
depends on a flag gets that flag's absence rather than a wrong one. Two gaps
this surfaced, both engine-side and both recorded here rather than worked
around: **`globalThis.queueMicrotask` does not exist in the engine** (Deno's
core JS uses it, and its `--enable-queue-microtask` flag is how V8 supplies one),
and there is no engine flag surface at all.

**Snapshot creation — landed, and it refuses.** `Isolate::snapshot_creator` /
`snapshot_creator_from_existing_snapshot`, `set_default_context`, `add_context`,
`add_context_data` and `OwnedIsolate::create_blob`, with
`v8::FunctionCodeHandling`. A creator isolate is a real isolate — a context can
be made in it, contexts and attached data are recorded, and the indices a host
stores are the positions it asked for — because the alternative to recording
them would be inventing them. What cannot happen is the blob:

> Slag has no snapshot format, so there is nothing to serialize a heap into and
> nothing that could read a blob back. `create_blob` therefore aborts with that
> reason rather than answering `None` (which the crate we stand in for's own
> callers unwrap, producing a worse message) or handing back a blob that boots
> nothing (which would be silent). `StartupData::is_valid()` answering `false`
> is the other half of the same statement, and the two are consistent: a host
> that needs a snapshot needs an engine that has one.

That is a deliberate scope decision, recorded in §9. The restore side landed with
it: `HandleScope::get_context_data_from_snapshot_once` and
`get_isolate_data_from_snapshot_once`, over the same seal the crate we stand in
for uses so the cast bounds stay unimplementable by a host. They answer
`DataError::NoData` for every index — the crate we stand in for answers that for
the *second* read of an index; here it is every read, which is the same statement
about a snapshot that was never made — so the restore path reports through the
error channel the shape already has rather than panicking.

Between them, the two steps moved the count by **one** (`FunctionCodeHandling`,
the only *name* in the group) and then by **zero**: every other item there was
already resolving as a method call, so it could never appear. The snapshot
subsystem is complete against what `deno_core` calls — verifiable by the bridge
tests, which mirror its call sites exactly (including
`get_context_data_from_snapshot_once::<v8::Data>` and the `Cow<[ExternalReference]>`
constructor) — while the count says nothing about it. That is the shape of the
frontier, not a problem with the work: names first, and the methods behind them
become countable only once every name resolves.

**The tail — landed.** The names that are one thing rather than one subsystem:
`json` (the engine's JSON, so parse/stringify are conversions), `MicrotasksPolicy`
with `Isolate::set/get_microtasks_policy` (the engine work above),
`PropertyDescriptor` (the engine's own partial descriptor, plus
`Object::define_property`, which is the non-throwing `DefineOwnProperty` the
crate we stand in for calls), `AllowJavascriptExecutionScope` (a marker: the
engine has no disallow-execution gate to restore), `latin1_to_utf8`,
`simdutf::validate_ascii`/`validate_utf16le` (the answers simdutf gives, without
the SIMD), `TYPED_ARRAY_MAX_SIZE_IN_HEAP` and the `ArrayBufferView` contents
accessors it speaks for.

Two of those are values rather than translations, and both say what the engine
really does: `TYPED_ARRAY_MAX_SIZE_IN_HEAP` is **0** because every view the
engine builds is backed by a buffer's own storage, so nothing lives in the
object; and `simdutf` exposes only the predicates, since a host that needs the
transcoders is told so at compile time by their absence rather than handed a
slower path. One quirk is reproduced rather than smoothed over: a value JSON
leaves out serializes to the *text* `undefined`, which is what the crate we
stand in for renders.

A third divergence is the microtask default: `Explicit` here, `Auto` there. The
engine has never drained a job nobody asked for, and a host that wants `Auto`
says so (a Deno-class host sets `Explicit` itself), so the default it inherits
is the behavior it can reason about.

**The inspector — landed, as a shape that refuses.** Slag has no debugger: no
breakpoints, no stepping, no protocol, and no second thread that could stop a
running script. V8's inspector is a subsystem inside the engine, which is why
this is the first thing in this plan that is *shape over nothing* rather than a
bridge over existing engine surface.

So `V8Inspector::create` aborts with that reason, and everything only reachable
through a created inspector does the same (`connect`, `context_created`,
`context_destroyed`, `create_stack_trace`, `exception_thrown`,
`dispatch_protocol_message`, `schedule_pause_on_next_statement`). Two
alternatives were rejected deliberately: a session that never answers (a debugger
hangs with no message), and a protocol that errors on every method (a host
cannot tell that from a bug). The *data* types are real and tested —
`StringView` (both unit widths, rendering the text rather than the units),
`StringBuffer`, `Channel` over a host's `ChannelImpl`, `V8InspectorClient` over a
host's `V8InspectorClientImpl`, and the trust level — so a host's own impls
compile against the shapes `deno_core` implements.

Three smaller pieces landed with it because the inspector is what needed them:
`UniquePtr` (the nullable form of `UniqueRef`; the inspector's `StringBuffer` is
handed around as one), the `StackTrace` tag, and `IsolateHandle` with the
termination and interrupt requests. Those answer `false` — "no request was
made" — rather than panicking, because the crate we stand in for treats them as
best-effort requests a host must check, and because `deno_core`'s own error path
calls `terminate_execution` on the way out of an unhandled exception, where an
abort would be an abort inside an abort. `Local::extend_lifetime_unchecked` and
its `ExtendLifetime` trait came with them too: a host that hands a context out of
a scope needs exactly that widening.

The count moved **78 → 42**, the largest step since the surface became
measurable, and for the reason the last two steps taught: these were all *names*.
`libs/core/inspector.rs` — 2000 lines of `deno_core` — now has no name-resolution
errors at all.

**Module records — landed.** `v8::Module` was the one thing every module-loading
path needed and the engine had no embedding representation for, so this step was
engine work first: `runtime::api::Module` exposes `compile`, `register`,
`status`, `has_top_level_await`, `requested_specifiers`, `requested_modules`,
`instantiate`, `evaluate`, `namespace`, `exception`, and `pin`. Two engine facts
it has to state rather than invent: the status vocabulary is the crate's own
(V8's `Errored` is "`Evaluated` with a recorded error", which is how a host
detects failure), and a module record is not a language value — no handle scope
holds it, so a persistent handle must pin it, which is why
`crux::heap::pin_handle` exists beside `pin`. The bridge then carries a record in
a fourth `Payload` variant and answers `get_status`, `get_exception`,
`get_module_namespace`, `evaluate`, `instantiate_module`,
`is_source_text_module` and `is_synthetic_module` from it;
`script_compiler::compile_module`/`compile_module2` compile through it. The
evaluation value is the engine's promise, which is what the crate's own callers
downcast to `Promise` — so that one is not a translation.

`slag` now re-exports the V8-shaped surface as `slag::api` (with a test that
drives a module through it), so a host porting from the `v8` crate depends on
the embedding entrypoint alone rather than on `runtime`.

**`cppgc` — landed, as an arena with no collector.** A host uses `v8::cppgc` to
keep Rust objects JavaScript retains: a value with a `trace` method, a JS
wrapper it is attached to, and handles that keep it alive from outside the
engine's heap. The engine's host-object seam is not it — `HostOps` dispatches
*behaviour* and carries no heap edges (the L2 row in §5) — so this is the
bridge's own heap, in `crates/v8/cppgc.rs`.

What is real and tested: allocation (the value really lives in a box the heap
owns), every handle type (`UnsafePtr`/`Member`/`WeakMember`/`Persistent`/
`WeakPersistent`/`GcCell`, reading and writing the pointer they name), tracing
(`Visitor::trace` really calls a host's `GarbageCollected::trace` and records
what it reached; `Heap::collect_garbage_for_testing` runs that mark from the
heap's roots), the JS association (`Object::wrap`/`unwrap` round-trip the same
pointer through a JS object, and `is_api_wrapper` answers it), and reclamation
at the end of the heap's life (`terminate`, or the drop that follows it, frees
every allocation and drops every value).

What is not: **the heap never collects.** Nothing is reclaimed before the heap is
terminated — `WeakPersistent::get` never observes a collection, and
`collect_garbage_for_testing` traces without sweeping. That is a decision, not an
unfinished edge, and the reason is the engine's: a sweep has to know every
pointer a host still holds, and V8's cppgc answers that by scanning the stack,
which Slag does not do for host memory (`crux::heap`'s scan finds its own boxes).
Freeing an object a stack `UnsafePtr` still names would hand the host a dangling
pointer — the aliasing hazard `crates/crux/src/heap.rs:16-18` warns about, one
level up. So a host that needs reclamation needs an engine that traces host
state, which is the L2 item above.

The JS association carries one divergence. V8 keeps a wrapped pointer in the JS
object's embedder fields, which JavaScript cannot see; this bridge has no such
field, so it defines an `External` under a bridge-owned property key — hidden
from `for-in`, `Object.keys` and `JSON.stringify` (non-enumerable, and
non-configurable so it cannot be removed), visible to
`Object.getOwnPropertyNames`. Nothing JavaScript can write can forge one, because
only the bridge can make an `External`. The key carries the tag, so unwrapping
with the wrong tag answers `None` the way the crate we stand in for does.

One isolate-level divergence comes with it: `Isolate::get_cpp_heap` always answers
`Some`. V8 answers null unless the embedder handed a heap to `CreateParams`
(`v8/src/heap/heap.h:2313`), and the host this is built for unwraps the answer
unconditionally, so an isolate here owns a heap whether or not one was passed —
and one *is* passed: `CreateParams::cpp_heap` is honored, with the host's heap
becoming the isolate's.

Verified as landed: **12 tests** in `crates/v8/cppgc.rs` (the heap's two free
paths, rooting and re-rooting, the mark phase following a `Member` edge and its
roots, a cycle terminating it, a `GcCell` traced through it, a wrap round-tripping
and the wrap's non-enumerability, a second wrap under one tag refused, the
isolate's heap, and the process sequence), `cargo test -p v8 --features simdutf`
**96 passed / 0 failed** (84 before), `cargo test --locked --workspace` **5,029
passed / 0 failed**, and `clippy --locked --workspace --all-targets -- -D warnings`
clean. No engine crate changed, so the test262 and wasm sweeps are not implicated —
the statement the inspector step made, and the one that holds here because
`crates/v8` is not in any runner's graph.

The count also behaved: **42 → 26**, exactly the 16 `cppgc` sites and nothing else.
That is worth stating because the opposite was equally likely — the cppgc error
masked `get_cpp_heap`, `Object::wrap`/`unwrap`/`is_api_wrapper` and the
`TryFrom<Local<Value>> for Local<Object>` that `try_unwrap_cppgc_with` does, and
all of them resolved. `libs/core/cppgc.rs` and the `webidl.rs` reference to
`cppgc::GarbageCollected` have no errors left at all.

**`FunctionBuilder` — landed.** The builder `deno_core` makes its op functions
and templates with: `new`/`new_raw`, `data`, `length`, `constructor_behavior`,
`side_effect_type`, and a `build` for each of `Function` and `FunctionTemplate`
(plus `FunctionTemplate::builder`/`builder_raw`, which `new`/`new_raw` are now
expressed in terms of). The line that matters is the one between the settings
that are *observable* and the one that is not.

- `length` and `constructor_behavior` are properties of the function itself, so
  both are real — over two parameters `Function::create_builtin` already took and
  the api layer did not expose. `api::FunctionTemplate` gained
  `set_length`/`set_constructible`, and `get_function` passes them, so a template
  built with `ConstructorBehavior::Throw` makes a function whose `new` throws
  (the engine's `construct: None`). That is `crates/runtime/src/api/template.rs`
  only: no `crux` change, and the defaults preserve every existing caller.
- `data` needed no engine change, and it fixed a defect. The bridge owns the
  closure the engine calls, so the builder pins the host's value as an L1
  `Global` and hands it to the callback view. Before this,
  `FunctionCallbackArguments::data` answered `undefined` for **every** function —
  the bridge passed `undefined` where the data belongs — so no Deno op could
  have read its `OpCtx`.
- `side_effect_type` is carried and not used, which is the honest tier rather
  than a gap: it tells V8's optimizer what it may assume about a call, and this
  engine has no optimizer, so no observable behaviour depends on it. Ignoring
  `length` would have been a lie; ignoring this is not.

Building a plain `Function` through a template is what the crate we stand in for
does, not a shortcut: V8's `Function::New` is a `FunctionTemplateNew` plus
`GetFunction` (`v8/src/api/api.cc:5690`).

**Verified, and one test had to be corrected for being unable to fail.** Five
tests in `crates/v8/function.rs`: the data reaching the callback, the data
surviving a collection, the length as a non-writable own property, `Throw`
refusing `new` while the default allows it, and the template path carrying the
same settings to its function. The collection test as first written asserted
straight after a `crux::heap` collect and passed *with the pin removed* — a swept
box keeps its bytes until something reuses the slot. It now churns 200 objects
through the free list before reading, and under that mutation it fails with
exactly the aliasing it exists to catch (`left: 3.0, right: 11.0`) — checked by
making the change, not by reasoning about it.

Gates: `cargo test -p v8 --features simdutf` **101 passed** (96 before),
`cargo test --locked --workspace` **5,034 passed / 0 failed**, `clippy --locked
--workspace --all-targets -- -D warnings` clean. `crates/runtime` *did* change
this time, so the sweep battery was re-run rather than argued: test262 `all`
48,464 pass / 0 fail / 0 crash / 0 hang of 48,622 (158 skip), `intl402` 3,205 / 0
of 3,357 (152 skip), wasm core **64,594 checks / 0 fail / 0 pending** across the
eight suites (20,662 + 7,485 + 105 + 654 + 8,709 + 912 + 77 + 25,990), and the
JS-API sweep **1,001 tests / 0 fail** (61 fixture-files, 45 pass, 16 skip). Every
number is the certified one.

**`deno_core` — inventory, and why the number moved the way it did.**

| step | `cargo check -p deno_core` errors |
|---|---:|
| start of this step | 124 |
| after the three scope macros (`tc_scope!`, `callback_scope!`, `escapable_handle_scope!`) | 75 |
| after `fast_api`, `ScriptOrigin<'s>` and the promise enums | 678 |
| after the callback ABI, `FunctionTemplate`, `Exception` | 678 → 292 |
| after `ExternalReference`, `OneByteConst`, the module/attribute enums | 292 → 145 |
| after `script_compiler`'s vocabulary (`Source`, `CompileOptions`, `NoCacheReason`, `CachedData`, `compile`, `compile_function`) | 113 |
| after `v8::Module` (`get_status`, `get_exception`, `get_module_namespace`, `evaluate`, `compile_module`) | **110** |
| after the resolver (`instantiate_module`, `ResolveModuleCallback`, `FixedArray`) | **110** |
| after initialization (`V8`, `Platform`/`PlatformImpl`/`Task`, `StartupData`, `CreateParams`) | **89** |
| after the tail (`json`, `MicrotasksPolicy`, `PropertyDescriptor`, `AllowJavascriptExecutionScope`, `latin1_to_utf8`, `simdutf`, `TYPED_ARRAY_MAX_SIZE_IN_HEAP`) | **79** |
| after snapshot creation (`SnapshotCreator`, `Isolate::snapshot_creator*`, `FunctionCodeHandling`) | **78** |
| after the snapshot restore side (method-level) | **78** (unchanged, and why is the point) |
| after the inspector (`v8::inspector`, `UniquePtr`, `IsolateHandle`, `ExtendLifetime`) | **42** |
| after `cppgc` (`v8::cppgc`, `Object::wrap`/`unwrap`/`is_api_wrapper`, `get_cpp_heap`) | **26** |
| after `FunctionBuilder` (`FunctionBuilder`, `data`/`length`/`constructor_behavior`, `FunctionTemplate::builder*`) | **20** |
| after the serializer (`ValueSerializer`/`ValueDeserializer`, the delegate and helper traits, this bridge's own wire format) | **8 names, and 617 type errors behind them** (see below) |
| after the first method pass (the handle casts and `Local`'s cross-type equality, the value conversions and element predicates, the object/array/function/promise/proxy/string calls, `NewTryCatch` from a callback scope) | **374** (see below) |
| after the `From`/`TryFrom` closure (the transitive casts, and the predicate a template cast needs) | **369** (see below) |
| after the scope and property bounds (`NewTryCatch` from a `ContextScope`, `PropertyFilter: Default`, and the `Proxy` getters they were hiding) | **364** (see below) |
| after the identity hashes (the `Hash`/`Eq` a host's tables key on, and the module identity they hang off) | **351** (see below) |
| after the context embedder-data slots (the two methods a host stashes its realm state through) | **347** (see below) |
| after private names (`Private::for_api`, the private read and write, and the cast to one) | **336** (see below) |
| after the method tail a host calls by name (`Global::open`, the typed `ReturnValue` setters, `Function::builder`, the promise resolver, and the pointer-shaped isolate slots) | **292** (see below) |
| after the scheduling and exception-control methods (`Isolate::perform_microtask_checkpoint` and `TryCatch::rethrow`) | **260** (see below) |
| after the isolate-level callback vocabulary (the promise-reject, prepare-stack-trace, `import.meta`, dynamic-import, phase-import and wasm-async-resolve callbacks, the near-heap-limit pair, `set_idle`, `set_bool`, `set_promise_hooks`, and `AsMut<Isolate>`) | **242** (see below) |
| after the symbol surface (`Symbol::for_key` and the eleven well-known accessors) | **233** (see below) |
| after the primitive array (`PrimitiveArray::new`/`length`/`set`/`get`) and the two typed integer accessors it surfaced (`Uint32::value`, `Int32::value`) | **225** (see below) |
| after the leftovers (the empty string and a one-byte const, `clear_all_slots`, `Promise::catch`, `get_property_names`, `has_pending_background_tasks`, and the `Global` raw-pointer pair) plus the continuation-value move it forced | **217** (see below) |
| after the buffer-handing shapes (`ArrayBuffer::new_backing_store_from_bytes` and its `Rawable` widths, and `SharedArrayBuffer::with_backing_store`) | **213** (see below) |
| after the host-memory store (`ArrayBuffer::new_backing_store_from_ptr`, the borrowed block the engine gained for it, and the `workers` shape that forced) | **209** (see below) |
| after the tag shape (`Local<'s, T>: Deref<Target = T>`, the tag receiver chain, and the `LocalHandle` the methods sit on until they move) | **62** (see below) |
| after the template surface a host fills in (the prototype and instance templates, `ObjectTemplate::{new, set}`, the two `String` conversions and `build_fast`) | **56** (see below) |

The count went *up* once because it had been lying: while a crate has unresolved
imports, rustc reports those and stays silent about the names behind them, so the
75 were only what some `use` statement happened to import. (The probe that
settled it: `let _: Option<v8::FunctionCallback> = None;` in a test of this crate
fails to compile, while `deno_core` names `v8::FunctionCallback` in a type alias
without a word.)

All 20 remaining are missing *names* (checked: every error is `cannot find ...`),
which is the shape of the frontier: the moment they resolve, the method-level
errors behind them surface, so the count is a lower bound until the last name is
declared. The inspector step is the counter-example that proves the rule — 36
names, gone in one step — while the snapshot steps moved 1 and 0, because
theirs were method-level. The last two steps are the clean case: `cppgc` was 16
sites, all of them names, and the count fell by exactly 16; `FunctionBuilder` was
6 sites and it fell by exactly 6 (5 in `runtime/bindings.rs`, 1 in `libs/core/
error.rs`, which is now clean). Nothing was hidden behind either, which is worth
stating because the *opposite* was equally likely both times.

**The serializer — landed, and the count stopped being a progress metric.**
`crates/v8/serialize.rs` carries `ValueSerializer`/`ValueDeserializer`, the two
delegate traits and the two helper traits, over a walk that round-trips
`undefined`/`null`/booleans, numbers as the `f64` itself (so `-0` and every NaN
survive), strings as UTF-16 code units (so a lone surrogate survives), BigInts,
arrays with holes written as holes, ordinary objects by their own enumerable
string keys, `Map`, `Set`, `Date`, `RegExp`, `ArrayBuffer`, typed arrays and
`DataView` — with object identity, so a cycle comes back a cycle and two
references to one object come back as one object. Host objects go through the
delegate's hooks and a shared array buffer through its transfer-id hook;
everything else (a function, a symbol, a promise, a proxy, a weak collection, an
iterator, an `Error`, a primitive wrapper, a detached buffer) fails through
`throw_data_clone_error`, which is V8's own path for a value it will not clone.
Four findings are worth more than the code.

- **Name resolution had been masking type checking.** The 20 "remaining" names
  were not 20 things left: while the crate has an unresolved *import*, rustc
  reports that and never type-checks a body. Resolving the serializer's six
  names let it through, and the count went **21 → 625** — 8 names (the wasm
  tail) and **617 type errors that had never been visible**. Every number in
  the table above counts *name* errors, and the way it fell by exactly the
  number of names added was a property of the phase, not of the work.
- **A quarter of those 617 are blocked by a shape difference, not by missing
  methods.** `rusty_v8` puts the methods on the *tag* types and gives
  `Local<'s, T>: Deref<Target = T>`, which is how `&v8::Value` is a receiver at
  all (`deno_core`'s `runtime/ops.rs:273` calls `is_string` on one) and how
  `&v8::Value → &v8::String` is a legal `transmute`. This bridge puts the
  methods on `Local<T>` and derefs along the tag chain instead, because its tags
  are zero-sized and a method on a tag has no payload to read. Measured:
  **78** `E0599`s have a tag receiver, and **75 of the 77** `E0308`s are
  `expected &Tag, found &Local<…>` — **153** errors that no number of added
  methods can close. The other **291** `E0599`s are on `Local<…>` and are the
  mechanical kind; **9** are `E0624`, `Local::cast` being `pub(crate)` here and
  `pub` there — and *checked* there (it panics on a bad cast) where this
  bridge's is an unchecked retag, so making it public is a decision rather than
  a one-word change.
- **The wasm module transfer-id hook is part of the delegate surface and
  unreachable**: the engine hands out no wasm module value for the walk to
  recognize. That is stated in the module header, not left to be discovered.
- **Two gaps in the walk**, both narrow and both in the module header: a
  property *key* that is not UTF-8 clean is spelled as U+FFFD by the bridge's
  own property enumeration (a *value* string is exact), and a malformed stream
  raises a pending `Error` where V8 raises a `DOMException` `DataCloneError`
  the engine has no type for.

Gates: `cargo test -p v8 --features simdutf` **118 passed / 0 failed** (17 of
them this step), `cargo clippy -p v8 --all-targets --features simdutf -- -D
warnings` clean, and the workspace: **5,050 passed / 0 failed / 4 ignored**
across 38 binaries, with one test skipped — see the crash below.
`crates/v8` is a workspace member and no runner depends on it, so the test262
and wasm sweeps are not implicated (checked in the Cargo manifests, not assumed).

**A pre-existing crash, found while gating this step.** The v8 test binary
aborts with `STATUS_ACCESS_VIOLATION` on most runs, and the test it dies in is
`function::tests::the_data_a_built_function_carries_survives_a_collection` —
the collection test the `FunctionBuilder` step above added. It passes 5/5 in
isolation and aborts about two runs in three when the whole binary runs, so what
it interacts with is what runs before it rather than the test alone. It is not
this step's (it reproduces with the serializer's tests skipped, and on HEAD's
tree, which has no serializer), and it is not fixed here: recorded because it
means `cargo test --locked --workspace` is *not* green as a whole — cargo stops
at that binary, so every count above is taken with it skipped — and because a
test that aborts instead of failing is the hazard §7's last step warned about
from the other direction.

What is left is 8 names, and behind them the type errors the table's numbers
now measure. The first pass through those, 617 → 374, is the step below.

**The method surface — first pass, 617 → 374, and what it fixed.** Five of the
things this added were not additions but *corrections*, which matter more than
the count:

- `Local::cast` was the bridge's unchecked retag wearing the crate's name. The
  crate's `cast` is checked and **panics** on a failed cast, and its unchecked
  form is the *associated* `cast_unchecked`; the retag is now a private `retag`,
  and `cast`/`try_cast`/`cast_unchecked` landed with the crate's semantics.
  Including the bound `Local<'s, A>: TryFrom<Self>` — which is satisfiable at all
  only through the blanket `TryFrom` over `From`, which is how the crate's own
  bound works.
- `strict_equals`/`same_value` took `&Local` where the crate takes `Local` **by
  value**, and `same_value` was aliased to strict equality — a wrong answer for
  `NaN` (false where it must be true) and for `-0`/`0` (true where it must be
  false). Both now ask the engine's own operations, and `same_value_zero` came
  with them.
- `Local`'s `PartialEq` was same-tag only. The crate's is
  `impl<T, Rhs: Handle> PartialEq<Rhs> for Local<'_, T>`, which is what lets a
  host write `value == object`; it is now that impl, comparing the payload (the
  bridge's tags carry nothing to compare), and `Global` has the matching one.
- `NewTryCatch` existed for a handle scope only, where the crate has five
  receivers — and `deno_core` opens a try-catch from a *callback* scope.
- `Object::has_own_property` came with the serializer step, because a `[[Get]]`
  cannot tell an array element from a hole.

The rest is additions, each against the crate's own signature: twelve
element-type predicates on `Value`; the value conversions (`to_object`,
`to_number`, `to_big_int`, `to_boolean`, `to_integer`, `number_value`,
`integer_value`, `int32_value`, `uint32_value`);
`Object::{set_index, delete, get_prototype, set_prototype, create_data_property,
define_own_property}`; `Function::{call, call_with_context, new_instance,
set_name}`; `Promise::{result, mark_as_handled, then2}` — `then2` through the
engine's own PerformPromiseThen, so a replaced `Promise.prototype.then` cannot
change what it does; `PromiseResolver::{resolve, reject}`; `Proxy::{get_target,
get_handler}`; `SharedArrayBuffer::get_backing_store`;
`String::to_rust_cow_lossy`.

**What the remaining 260 are, measured.** By error kind: **169** `E0599`s, **78**
`E0308`s, **8** names, **3** `E0515`s and **1** `E0282`. The `E0599`s split by the
receiver each message names: **41** on a `Local<…>`, **106** on a bare tag
(`Value` 53, `String` 9, `Symbol` 9, `ArrayBuffer` 6, `BigInt` 6, `PrimitiveArray`
4, and one or two for each of the rest) — the shape §9 records as open, since the
methods live on the tag there — and **22** on another bridge type (`OwnedIsolate`
11, `Exception` 3, `PinnedRef` 3, `Global` 2, and three single sites). The
`E0308`s are the other half of the tag shape's toll, and the `E0515`s are
scope-lifetime sites. Not one of them is a name to declare.

**The `From`/`TryFrom` closure — landed, and eleven casts deliberately absent.**
The crate's cast tables are transitive closures, not direct-edge tables, so a
host's `Local<Data>: From<Local<Function>>` and
`TryFrom<Local<Data>> for Local<FunctionTemplate>` have to exist. `impl_from!`
reached the crate's 176 pairs at the end of the previous step; `impl_try_from!` is
now **163 of the crate's 173**, checked by diffing the two lists rather than by
reading them. Every one of the ten left out needs a predicate the bridge
cannot honestly answer with, which is the rule `TagCheck` already states — a cast
whose answer would be a guess is a compile error instead:

| absent cast | why |
|---|---|
| `Data`/`Template` → `ObjectTemplate` | the bridge mints no object template yet, so nothing can be one |
| `Data` → `FixedArray` | a `FixedArray` here is the JS array the bridge built for the host, so `is_array` would answer `true` for every array |
| `Data` → `ModuleRequest` | there is no payload or table for one here; `Module` and `Private` both have one, which is why those pairs landed |
| `Data`/`Value`/`Object` → `WasmMemoryObject`, `WasmModuleObject` | the bridge builds no wasm object, and `crates/wasm` is not in its graph |

The two pairs `deno_core` actually asks for are both real answers. `Data =>
Module` reads the payload the module handle already carries: a module record is
not a language value, so it has no encoded form a `Value` payload could hold, and
the predicate is exact rather than a heuristic. The template family needed one
thing more. A template has no engine object of its own, so a handle *is* an
`External` naming the address the isolate took the template under — and nothing
about an `External` says whether it is a template, because a host wraps its own
pointers in the same shape. The predicate is therefore that address question
asked of the isolate that owns it: `Isolate::owns_template` over
`IsolateInner::templates`, reached from the value through the entered realm
(`realm::current().isolate()`) by the same `Isolate::from_engine_ptr` the
bridge's `GetIsolate` already uses, resting on the layout assertion that keeps an
engine pointer and a handle naming one address. With no realm entered the answer
is `false`, as it is for every other table predicate here. What is *not* claimed:
`Template` and `FunctionTemplate` share that predicate today, because the only
templates this bridge mints are function ones, and the first object template has
to widen both.

The test guarding it can fail, and was made to: the data a template was
registered under casts back to a `FunctionTemplate` — including across a `Global`
round trip, the shape a host's snapshot machinery keeps one in — while an
`External` around a host pointer does not; with the predicate forced to `true`
the second assertion fails, which is how it was checked.

**The measurement, four ways.** The last step recorded 374 for this tree, and
374 is exactly this tree *without* this step's four pairs *and* without the
transitive `impl_from!` block that step landed last. With the block and without
the pairs it is 371, with the pairs and without the block 372, with both **369** —
so the previous step's closure is worth 3 of the difference and this step's pairs
exactly 2, which are the two `TryFrom` bound errors `deno_core`'s
`SnapshotLoadDataStore::get::<T>` raises (its bound is literally
`Local<'s, T>: TryFrom<Local<'s, Data>>`, and the two `T`s it asks for are
`Module` and `FunctionTemplate`). No other error appeared or vanished in either
measurement, checked by diffing the two error lists rather than by the count.

Gates: `cargo test -p v8 --features simdutf` **119 passed / 0 failed** (the new
cast test included; the feature is what the last steps used), `clippy` clean at
crate and workspace scope, and
`cargo test --locked --workspace` **5,051 passed / 0 failed / 4 ignored** with
the crashing test skipped (a re-run: the first run failed on the `runtime` flake
below, which aborts cargo and so hides every later binary's count).

**The scope and property bounds — landed, 369 → 364, and one error that had been
hiding behind them.** Three things were missing, and the first needed a shape
correction before it could be added at all:

- **`NewTryCatch` had two receivers where the crate has five.** `deno_core`
  opens its try-catch through its own `context_scope!` macro, so what it hands
  `TryCatch::new` is a `&mut ScopeStorage<ContextScope<..>>`; the crate's
  `TryCatch::new(param: &mut P)` reaches its `ContextScope` impl through
  `ScopeStorage`'s `DerefMut`. That only works because the crate's
  `ContextScope::new` returns the scope itself — this bridge wrapped it in
  `ScopeStorage`, so `P` unified to `ScopeStorage<..>`, the `ContextScope` impl
  was never reached, and the bound failed on the storage. So: `ContextScope::new`
  now returns the scope (a context scope is not address-sensitive — it borrows the
  scope it wraps — and the new doc comment says so), and the `ContextScope`
  receiver was added. The crate's other two — an escapable handle scope and a
  nested try-catch — stay absent until a host asks.
- **`PropertyFilter: Default`** is `ALL_PROPERTIES`, written out rather than
  derived, as there.
- **`Proxy::get_target`/`get_handler`** returned `Local<'_>` elided to `&self`,
  which is the receiver rule: a caller that reads a target inside an `if let`
  then holds a result borrowed from the `proxy` binding (deno's
  `libs/core/webidl.rs:706`). The crate takes the scope's lifetime; the two
  getters and the `slot` behind them now do too.

**Measured: five bounds out, one error in.** Putting the tree back the way it was
before those changes leaves it at 369 with five `E0277`s; landing them removes
exactly those five and reveals one `E0597` — the `Proxy` lifetime above, which the
failing bound had been masking, the same effect that made the count jump when the
last names resolved. Fixing that borrow is the fourth change, and only then is the
count 364 with **no `E0277` left anywhere in the `deno_core` check**. `deno_core`'s
own total goes 450 → 446.
What is left, by kind and by count: **271 `E0599`** (51 on a `Local<…>`, 122 on
a tag, 85 on another bridge type, 13 bound failures), **78 `E0308`** (the tag
shape §9 records as open), **8 names** (the wasm tail and the three stragglers,
one of them an `E0433` rather than an `E0425`), **3 `E0515`**, **2 `E0605`** and
**1 `E0282`**.

Gates for this step: `cargo test -p v8 --features simdutf` **119 passed / 0
failed** (118 with the crashing test skipped), `clippy` clean at crate and
workspace scope, and `cargo test --locked --workspace` **5,051 passed / 0 failed
/ 4 ignored** across 38 binaries with that test skipped.

**Scheduling and exception control — landed, 292 → 260.** Two methods, and both
are the crate's names for things the bridge already did under its own:

- **`Isolate::perform_microtask_checkpoint` (25 sites, and 31 of the resolved
errors were this one's `PinnedRef` receivers).** The crate's method runs the
  default microtask queue until it is empty. The bridge already had
  `run_microtasks` as an accessor that *returns* a job's error; the checkpoint is
  the crate's name for the same call with the crate's error policy — and there
  the divergence is the one this bridge records for `Auto` (a job that throws
  becomes the pending exception, where the crate swallows it), so it reuses the
  bridge's own `throw` rather than inventing a second copy of the conversion.
  The sites reach it through the deref chain (`scope.perform_microtask_checkpoint()`
  is `Isolate`'s method, as in the crate).
- **`TryCatch::rethrow` (8 sites, on the handler view).** The engine observes the
  isolate's one pending-exception slot and clears it when a handler drops —
  unless the handler was rethrown, which is the flag the engine keeps for exactly
  this. What the bridge adds is the crate's *signature*: it hands the
  propagating value back, so a host can rethrow and use the value together.

Two things the tests pinned down, both worth having in writing. A handler sees
what is thrown *inside* it, not what was pending when it opened — opening one
takes the pending exception aside, which is the engine's model and the reason a
handler's `has_caught` is about its own lifetime. And a rethrow leaves the
exception in the slot past the handler that caught it, so a handler opened around
that one sees it; the test asserts the slot rather than a second `TryCatch`,
because opening one takes the exception aside again.

Measured: **292 → 260**, all of it `E0599` (201 → 169); the kinds beside it are
unchanged.

Gates: `cargo test -p v8 --features simdutf` **131 passed / 0 failed** (130 with
the crashing test skipped), `clippy` clean at crate and workspace scope, and
`cargo test --locked --workspace` **5,064 passed / 0 failed / 4 ignored** across
38 binaries with that test skipped. `crates/v8` only, so no sweep is implicated.
Both new tests were made to fail: removing the rethrow call from `rethrow` fails
the first, and a checkpoint that does not run the queue fails the second.

**The isolate-level callback vocabulary — landed, 260 → 242, and exactly one of
them fires.** Eight `OwnedIsolate` `E0599`s, the near-heap-limit pair, and four
stragglers that were method-level rather than shape-level. Most of the step is
vocabulary — a host passes a *callback type*, so the types had to exist before the
methods could — and every entry needed the tier decision §9 states, because "the
engine has the event" is true of one of them:

- **`set_promise_reject_callback` — implemented, engine-backed.** The engine
  already reports a promise rejected with no handler, and a handler arriving for
  a rejection it has already reported, through
  `HostHooks::promise_rejection_tracker`. The bridge installs its own
  implementation of that seam on the isolate's agent (`PromiseRejectHooks`,
  `crates/v8/isolate.rs`), which maps the engine's two events onto
  `PromiseRejectEvent` and calls the host. The seam is *one* trait for every
  host-defined operation, so the bridge owns it for the isolate — and an isolate
  whose host never calls this keeps the engine's defaults, which is what having
  no hooks at all does. Two divergences, both in the doc comment: the engine
  reports a rejection as it happens where V8 waits for the microtask checkpoint,
  and the rejection value is present only when the engine had it in hand.
- **The other seven — accepted, recorded, not fired.**
  `set_prepare_stack_trace_callback`, `set_host_initialize_import_meta_object_callback`,
  `set_host_import_module_dynamically_callback` (and its phase twin),
  `set_wasm_async_resolve_promise_callback`,
  `add`/`remove_near_heap_limit_callback`, `set_idle`, and
  `set_capture_stack_trace_for_uncaught_exceptions`, each with a doc comment
  saying what the engine does instead: it builds error stacks itself (script-level
  `Error.prepareStackTrace` is what replaces them), builds `import.meta` at load,
  resolves a dynamic import from the modules the host registered (`crate::module`'s
  header), compiles wasm synchronously, and neither enforces a heap limit nor
  profiles. A host that needs one of these to fire needs the subsystem behind it,
  not the name.
- **`HandleScope::set_promise_hooks` — accepted, not run**, for the same reason:
  nothing in the engine fires a host hook when a promise is created or settled.
- **`ReturnValue::set_bool`** was a gap rather than a shape, and
  **`AsMut<Isolate> for OwnedIsolate`** (and for `Isolate`) is what makes
  `manually_drop.as_mut()` resolve — how deno's allocator reaches its isolate.

Two shapes diverge deliberately and are recorded in §9: `PromiseRejectMessage`
carries the engine's values instead of a pointer to V8's own struct, and
`PrepareStackTraceCallback` is the *host function's* shape rather than the C
pointer whose return value travels through the platform's ABI.

**The streaming callback is deliberately not in this step.** Its argument type
(`WasmStreaming<false>`) and the setter it is passed to
(`set_wasm_streaming_callback`, `runtime/setup.rs`'s last error) both need the
streaming subsystem: `crates/wasm` decodes a whole binary, and what is missing is
the byte-at-a-time API and the promise it resolves. Declaring a hollow
`WasmStreaming` here would move errors rather than close them — four method
`E0599`s in `ops_builtin{,_v8}.rs` in place of two names — so it goes with the
wasm tail, where item 2 of the survey already put it.

Measured: **260 → 242**; `E0599` **169 → 155**, names **8 → 4** (the two
`WasmStreaming` sites, `SyntheticModuleEvaluationSteps` and
`CompiledWasmModule`), `E0308` 78 unchanged, and the four stragglers (3 `E0515`,
1 `E0282`) unchanged. Every `OwnedIsolate` site but the streaming one is closed
(11 → 1), and `runtime/setup.rs`, which held eight of them, is down to that one.

The 155 `E0599`s now split by receiver: **72** where a *value handle* is the
receiver (`&Value` 53, `&String` 7, `&BigInt` 6, `&Number` 2, and four singles) —
the tag shape §9 records as open — **41** on a `Local<…>`, **38** associated
items on a tag or type name (`Symbol` 9, `ArrayBuffer` 6, `PrimitiveArray` 4,
`Exception` 3, and the rest), and **4** on another bridge type (`PinnedRef` 2,
`OwnedIsolate` 1, `FunctionBuilder` 1).

Gates: `cargo test -p v8 --features simdutf` **135 passed / 0 failed**, `clippy`
clean at crate and workspace scope, and `cargo test --locked --workspace`
**5,067 passed / 0 failed / 4 ignored** across 38 binaries with the crashing
`v8` test skipped. `crates/v8` only — no suite in this workspace links it (the
engine crates do not depend on it; checked in their manifests, not assumed) — so
no sweep is implicated. The firing test was made to fail: with the `host_hooks`
install removed it reports an empty event list.

**The symbol surface — landed, 242 → 233, and it is the registry script uses.**
Nine sites were `Symbol::for_key` (eight) and `Symbol::get_iterator` (one), and
the engine already had both halves: `Agent::global_symbol_registry` *is* the list
`Symbol.for` reads, and `crux::symbol::well_known` is what the engine installs
`Symbol.name` from. So the bridge calls those tables rather than keeping its own:
a name registered from either side is the one symbol both sides get, and a
bridge-held symbol used as a property key is the key a script's own lookup finds.
Two things the tests pin down, because both would pass with a
plausible-looking wrong implementation. A bridge that minted a fresh symbol for a
name would still return *a* symbol whose description matched, so the test asserts
identity with what script holds — in both directions, and twice for one name. And
a bridge that minted its own "well-known" symbol would still make `arr[key]` work
when the key came from the bridge, so that test asserts identity with
`Symbol.iterator` first and then reads *through* the key.

The description is the string's own code units rather than the crate's lossy text
conversion: two names a lone surrogate tells apart would otherwise fold into one
registry entry. The accessors are the eleven the crate we stand in for has, no
more — `Symbol::new`, `Symbol::for_api` and `Symbol::description` stay absent
until a call site asks, which is the demand rule §9 states.

Measured: **242 → 233**; `E0599` 155 → 146, every other kind unchanged, and no
new error surfaced behind the nine.

Gates: `cargo test -p v8 --features simdutf` **137 passed / 0 failed**, `clippy`
clean at crate and workspace scope, and `cargo test --locked --workspace`
**5,069 passed / 0 failed / 4 ignored** with the crashing `v8` test skipped.
`crates/v8` only, so no sweep is implicated. Both tests were made to fail:
replacing the registry lookup with a fresh symbol fails the first, and minting a
fresh "well-known" symbol fails the second.

**The primitive array — landed, 233 → 225, and two width gaps behind it.** Eight
sites were `PrimitiveArray::new` (four), `length` (two) and `get` (two), plus the
`set` calls that were masked behind those. The engine has no primitive array, so
what a host writes is the JavaScript array the bridge built for it — the decision
`FixedArray` already records, and a close one, because V8's `PrimitiveArray` is
itself an `Array` subclass. The shape is the crate's exactly, read off
`deno_core`'s call sites and the reference's `primitive_array.rs` rather than
recalled: `new`, `length`, `set` returning nothing, and `get` answering a
`Primitive` rather than an `Option`.

Landing it surfaced two real gaps, both in the typed-integer accessors: the bridge
had `Integer::value() -> i64` and no `Uint32::value`/`Int32::value`, so deno's
`Uint32::value()` resolved through the deref chain to the *signed 64-bit* read and
`Some(int.value())` stopped being a `u32`. The width is the whole of what those
accessors carry here — the engine has one number kind — so they came with the
slice: a `Uint32` read that answered like `Integer` gives a wrong answer for any
value above `i32::MAX`, and the test asserts exactly that case.

Measured: **233 → 225**. The eight `E0599`s closed, the two `E0308`s the typed
reads surfaced closed with them, and the kinds are back where they were before
this step (`E0599` 146 → 138, `E0308` 78, names 4).

Gates: `cargo test -p v8 --features simdutf` **140 passed / 0 failed**, `clippy`
clean at crate and workspace scope, and `cargo test --locked --workspace`
**5,072 passed / 0 failed / 4 ignored** with the crashing `v8` test skipped.
`crates/v8` only, so no sweep is implicated. All three new tests were made to
fail: dropping the requested length, no-opping `set`, and truncating the `u32`
read each fail the assertion guarding it.

**The leftovers — landed, 225 → 217, and two corrections behind them.** Eight small
accessors with no subsystem behind them: `String::empty`,
`String::new_from_onebyte_const`, `Context::clear_all_slots`, `Promise::catch`,
`Object::get_property_names`, `Isolate::has_pending_background_tasks`, and the
`Global::into_raw`/`from_raw` pair. Three carry a decision worth reading:

- **`get_property_names` made the shared key walk honest.** Both enumeration calls
  now run one walk over a *list* of objects, because the difference between them
  is which objects are visited. Two rules came with it, read off V8's
  `KeyAccumulator` rather than guessed: a key is reported where it is *first*
  seen, and a key the *attribute* filter rejects still counts as seen — which is
  what makes a non-enumerable own property hide an enumerable one up the chain.
  The kind and index filters do not count, because V8 drops those before its
  shadowing bookkeeping. The `index_filter` had been **ignored entirely**: the
  host's serializer asks for `SkipIndices` and was getting the indices back.
- **`Global::into_raw`/`from_raw` is ownership through a pointer.** The box
  `into_raw` leaks is what `from_raw` takes back, and a handle left out leaks
  rather than pinning anything — the crate's contract with a different noun for
  the resource. The isolate argument is unread here, and says so.
- **`has_pending_background_tasks` answers `false`**: a background task is work
  posted to another thread and this engine posts none (§9's platform decision,
  seen from the isolate). The test sits next to a *queued job* so the assertion
  states the distinction rather than just the constant.

Two corrections the step forced, both from the host reaching code the errors had
been hiding:

- **The continuation-preserved value belongs on the scope.** The bridge had
  `set`/`get_continuation_preserved_embedder_data` on the isolate; the crate has
  them on the handle scope, and the difference is a borrow error at the host:
  `let cped = scope.get_...()` followed by `tc_scope!(let t, scope)` cannot
  borrow-check when the handle is tied to the borrow of the scope instead of the
  scope's own lifetime. The pair moved; the isolate still stores the value, and
  the new test holds it across a `&mut` use of the same scope.
- **Two `E0502`s appeared and left.** They were *behind* `Promise::catch` in the
  same function, so the borrow check only ran once that method resolved — the same
  masking §7 records for name resolution, one layer down.

Three items were surveyed and left, each needing something the engine does not
have: `Object::get_constructor_name` (V8 answers from its *map* —
`new_target_is_base`, `is_prototype_map`, the map's constructor slot,
`FunctionTemplateInfo::class_name` — with `Symbol.toStringTag` as a fallback, so a
walk over properties would answer a different string for exactly the objects a
host shows it), `Context::get_extras_binding_object` (the host reads `console` out
of it, so an empty object would be a wrong answer rather than a missing one), and
`Isolate::get_heap_statistics` (the engine counts boxes, not bytes).

Gates: `cargo test -p v8 --features simdutf` **148 passed / 0 failed**, `clippy`
clean at crate and workspace scope, and `cargo test --locked --workspace`
**5,080 passed / 0 failed / 4 ignored** with the crashing `v8` test skipped.
`crates/v8` only, so no sweep is implicated. Every new test but one was made to
fail: swapping `catch`'s handler to the fulfilling side, ignoring the collection
mode, inverting the index filter, and stubbing `from_raw` each fail the assertion
that guards them. The exception is the continuation-value test, whose content *is*
the borrow shape: it fails to compile if the handle goes back to borrowing its
scope.

**The template surface a host fills in — landed, 62 → 56, ten sites fixed and four
revealed.** Ten of the errors were methods the engine already backs, so this was
bridge work: `FunctionTemplate::{prototype_template, instance_template}` (3 sites),
`ObjectTemplate::{new, set}` (3), `String::{write_utf8_into, is_onebyte}` (3) and
`FunctionBuilder::build_fast` (1). Four more were *behind* those — resolving a
receiver makes the next line's error appear — which is why the net is six:
`ObjectTemplate::{new_instance, set_with_attr}` and `FunctionTemplate::{set,
inherit, set_accessor_property}` are the rest of the same cluster.

- **`ObjectTemplate::new` had to go on the tag.** A host writes it as
  `v8::ObjectTemplate::new(scope)` — a tag-scoped call, not a method on a handle —
  so an inherent impl on `LocalHandle` does not answer it, and the first cut of
  this was on the handle. `FunctionTemplate::new` was already on the tag for the
  same reason; the rest of the cluster is called by method syntax, where the
  tag's deref chain reaches the handle.
- **An object-template handle is an `External` over an address the isolate
  keeps**, the same shape a function template's handle has. It needed a registry
  of its own beside the function templates, because a handle names the address it
  was registered under and the two kinds are then told apart by which list holds
  it.
- **`build_fast`'s overloads are accepted and not used**, stated rather than
  silent: a fast call is V8's own compiled entry point for a host function and
  there is no compiler here to generate one, so a call reaches the callback the
  builder was given — which is also where the crate we stand in for lands when a
  fast call does not apply. The test hands it a real overload slice and asserts
  the callback still runs.
- **`is_onebyte` is exact here where the crate's is a hint.** There the answer is
  read off the string's representation and may say `false` for a Latin-1 string;
  here it is `contains_only_onebyte`'s answer. That is a refinement of the same
  promise rather than a different one — `false` remains correct for a string that
  is not Latin-1, and `true` is now a fact rather than a guess.
- **`ObjectTemplate::set` panics on a non-string key.** The engine's templates are
  string-keyed, the crate gives that method no channel to report on, and the
  alternative to the panic is a property that silently does not exist.

Measured: **62 → 56**, `E0599` 53 → 47. Five new tests, all in `crates/v8`: the
prototype and instance templates reaching the objects they describe (a
prototype-template method called on an instance, an instance-template property
read off it), an object template standing on its own, `write_utf8_into` replacing
what its buffer held (including a lone surrogate becoming U+FFFD) and `is_onebyte`
either way.

Gates: `cargo test -p v8 --features simdutf` **157 passed / 0 failed** with the
crashing test skipped (it aborted a full-binary run during this pass as well,
which is the documented pre-existing crash), `clippy --locked --workspace
--all-targets -- -D warnings` clean, and `cargo test --locked --workspace`
**5,092 passed / 0 failed / 4 ignored** across 38 binaries. `crates/v8` only, so no
sweep is implicated.

**The tag shape — landed, 209 → 62, and the three decisions it forced.** `Local<'s, T>`
is now `#[repr(C)] { payload, marker }` with `Deref<Target = T>`, so a tag
*reference* is a receiver: a `&v8::Value` calls the methods a
`v8::Local<v8::Value>` has, and the reference the deref hands out is the handle's
own payload — which is what lets the host reinterpret it (`&v8::Value` →
`&v8::String` after `is_string`, `libs/core/runtime/ops.rs:265-278`) and read
through the result.

- **Where the methods live.** Rust allows one `Deref` per type and `Local`'s is
  now `Target = T`, which leaves the inheritance chain and the 175 methods that
  hung off it nowhere to sit. They moved to `LocalHandle<'s, T>` — the same
  `#[repr(C)]` layout, carrying today's methods and today's `derefs_to!` edges —
  and each tag derefs into it (`T: Deref<Target = LocalHandle<'static, T>>`), so a
  `&T` reaches exactly what a `Local` reaches. That was a rename of 27 impl
  blocks across 15 files plus the deref table, not a rewrite: **the bodies are
  unchanged**, because the accessors they read moved with them. It is
  scaffolding — the end state is the crate's, methods on the tags, `LocalHandle`
  gone — and with it in place those moves are now incremental, file by file.
- **The receiver is `&self` and the type the methods live on is not `Copy`.**
  Thirteen methods took `self` by value (`Module`'s accessors, `FixedArray`'s two)
  because a `Local` is `Copy`; on the tag they take `&self`, which is the crate's
  shape and the permissive direction for callers. `LocalHandle` is deliberately
  *not* `Copy`: the crate's tags mostly are not, and a `Copy` receiver is what
  makes `-D warnings` fire on all twelve `to_*` methods taking `&self`.
- **Two call sites lost a coercion and gained the crate's own conversion.**
  `&array` where `&Local<Object>` was wanted used to work by deref coercion
  through the old `Local`-to-`Local` chain; `.into()` is what that call is in the
  crate we stand in for, and the `impl_from!` table already had it.

Measured: **209 → 62**, and the change is exactly the shape's part of it — all 78
`E0308`s are gone and `E0599` falls 122 → 53. What remains is 53 named methods and
subsystems (the message and stack-trace surface, synthetic modules, wasm
streaming, code cache, source offsets, the extras binding object), the 4 names,
and four stragglers (3 `E0515`, 1 `E0282`).

Gates: `cargo test -p v8 --features simdutf` **153 passed / 0 failed** (one new),
`clippy --locked --workspace --all-targets -- -D warnings` clean, and
`cargo test --locked --workspace` **5,087 passed / 0 failed / 4 ignored** across 38
binaries with the crashing `v8` test filtered out. No engine crate changed —
`git status` names `crates/v8` only — so no sweep is implicated.

The new test is `tests::a_tag_reference_is_a_receiver`: it takes a `&v8::Value`
from a handle, asserts the payload behind it is the handle's own (pointer
equality — that is the runtime half), then reinterprets it as a `&v8::String` the
way the host does and reads one `String` method and one declared four tags up.
Its guard is partly a *compile*-time one, and honestly so: removing the deref
stops it compiling, and the payload's address is the design, so the mutation that
would move it is not one that can be staged soundly (a `static` payload is not
`Sync`, and a shifted address is UB).

**The host-memory store — landed, 213 → 209, and the first engine change the
bridge forced.** Four sites were `ArrayBuffer::new_backing_store_from_ptr`
(`libs/core/runtime/jsruntime.rs:1792/1824/1855` and
`libs/core/runtime/ops_rust_to_v8.rs:374`), the aliasing constructor: there, the
host's bytes are read where they are and freed by `deleter_callback` when the
store dies, and there is no way to fake it. A store that copied the bytes would
answer a different address and, worse, would leave the host's allocation
untouched by a script writing through a buffer over it — which is the one thing
a host uses this constructor for. So the engine grew a block over memory it does
not own: `SharedBuffer::borrowed(data, byte_length, deleter)`, whose accesses
reach the host's allocation in both directions and whose deleter runs when the
last clone goes. Three things made it more than a field.

- **The state box already points at the right place.** `BlockState::data` is the
  live byte base the JIT's inline element store reads through, and for a
  borrowed block it is the host's address, set once because host memory does not
  move. That is why the JIT inline needed no special case, and it is why `resize`
  refuses rather than reallocating memory the host is still holding.
- **The block's shape is one shape, not two.** The first cut put the borrowed
  field behind `cfg(not(feature = "workers"))` — under `workers` a block is an
  atomic word array, and a host's byte range is not one. That compiled in
  isolation and left the *workspace* not type-checking: `crates/test262` enables
  `runtime/workers`, feature unification puts it in the same graph, and
  `crates/v8` then asked for a constructor that build had compiled out (one
  `E0599`, `cannot find function borrowed`, with the workspace check failing on
  it). The fix is the honest one rather than a `cfg` on the bridge: the borrowed
  block exists in both builds, and what differs is what an *atomic* operation on
  one does — an owned block under `workers` hands the operation to the machine,
  a borrowed block takes the plain path it already took single-agent, because
  the host's bytes are bytes. The dispatch is a shared early return in each
  accessor (`byte_length`, `block_id`, `read`, `read_into`, `write`, `resize` and
  the three `atomic_*`), so there is one body per operation and no second,
  cfg'd copy to drift.
- **`Send + Sync` for a borrowed block is stated, not implied.** Under `workers`
  a `SharedBuffer` is moved to a worker thread
  (`crates/runtime/src/workers.rs:22`), so the `Arc<BorrowedBlock>` needs the
  bounds. They are `unsafe impl Send`/`Sync` with the reason written down: what a
  host passes is a C function pointer and registration data, which is `Send` in
  fact and cannot say so to the compiler — the same assertion
  `deno/ext/ffi`'s `BackingStoreHolder` makes about the same shape. The
  constructor's `# Safety` section and the block's doc comment both carry the
  consequence: a host must not share a borrowed block between agents.

The bridge's half is four lines (`crates/v8/array_buffer.rs`): the C deleter
becomes the closure the block runs, so `deleter_callback(data_ptr, byte_length,
`deleter_data`) fires exactly when V8 fires it.

Measured: **213 → 209**, all of it `E0599` (126 → 122) and re-measured from
`deno/` as 209. The regression this correction removed is not in that number and
is why it is worth recording: `cargo check -p v8 -p test262` is the probe for the
unified build, and it failed on the single `E0599` above until the shape was
unified.

Gates: `cargo test -p byteblock` **2 passed / 0 failed**, and the same **2
passed** under `--features workers` — the borrowed tests are no longer `cfg`'d
out of that build, which is where the new path is; `cargo test -p v8 --features
simdutf` **152 passed / 0 failed**; `clippy --locked --workspace --all-targets --
-D warnings` clean; `cargo test --locked --workspace` **5,086 passed / 0 failed /
4 ignored** across 38 binaries with the crashing `v8` test filtered out (5,082
before: the two `byteblock` borrowed tests were outside a workspace build and now
run in one, and the two `v8` host-memory tests are this slice's). The borrowed
path's guard was made to fail rather than argued: replacing the hoisted
`atomic_load` branch with `return Ok(0)` fails
`a_borrowed_block_is_the_hosts_bytes` in the `workers` build (`left: 0, right:
5`) — which is what says that build takes the borrowed path and not the
word-array one. The deleter counter in the second test is an `Arc<AtomicUsize>`
for the same reason: under `workers` the block is `Send`, and a test whose
dealter was an `Rc` would contradict the type it builds.

`byteblock` is an engine crate every runner links, so the sweep battery re-ran
rather than being argued, and every number is the certified one: test262 `all`
**48,464 pass / 0 fail / 0 crash / 0 hang** of 48,622 (158 skip) — with none of
the three `copyWithin` deadline hangs this time — `intl402` **3,205 / 0 / 0 / 0**
of 3,357 (152 skip), the eight wasm core sweeps **64,594 checks / 0 fail / 0
pending** (20,662 + 25,990 + 77 + 7,485 + 105 + 654 + 8,709 + 912), and the
JS-API sweep **1,001 tests / 0 fail**.

**The buffer-handing shapes — landed, 217 → 213, and the engine left alone.** Four
sites were `ArrayBuffer::new_backing_store_from_bytes` (two) and
`SharedArrayBuffer::with_backing_store` (two). Everything the sites do around those
— `UniqueRef::make_shared` (which is `Rc`-wrapping here, and already existed),
`ArrayBuffer::with_backing_store`, `Local::cast_unchecked` — was already in the
bridge, which is why the count fell by exactly the four rather than by eight: this
is what the moment the errors resolve is for.

Two decisions came with it, both visible in the doc comments:

- **`new_backing_store_from_bytes` reads the bytes into an engine block.** There,
  the bytes are taken by value and V8 reads them where they are, freeing them with
  the store. Here the caller's buffer is released as the call returns, so the
  length and the contents are what a host gets and the *address* is not. The
  `Rawable` trait is the widths that difference makes load-bearing: `Box<[u16]>`
  is two bytes per element, so a bridge that used the element count would hand
  script half the bytes it asked for — which is the mutation the width test was
  made to catch.
- **`SharedArrayBuffer::with_backing_store` marks the *block* shared as well as
  the buffer record.** The engine's language-facing constructor does both; its
  host-facing one (`shared_array_buffer_from_block`) records only the buffer, so
  a host asking the store afterwards would be told it is unshared. The bridge
  compensates rather than changing an engine function with three other callers
  (wasm memory, workers, the test262 runner) — recorded as an open decision
  rather than fixed in passing.

Measured: **217 → 213**, all of it `E0599` (130 → 126), and no error appeared
behind the four.

Gates: `cargo test -p v8 --features simdutf` **150 passed / 0 failed**, `clippy`
clean at crate and workspace scope, and `cargo test --locked --workspace`
**5,082 passed / 0 failed / 4 ignored** with the crashing `v8` test skipped.
`crates/v8` only, and no engine crate was touched, so no sweep is implicated.
Both new tests were made to fail: an element count where a byte length belongs
fails the first, and dropping the `mark_shared` fails the second.

**The method tail — landed, 336 → 292, one real bug found, and a pointer shape
corrected.** Forty-three sites were methods a host calls by name that the bridge
did not have at all:

- **`Global::open` (22 sites)** — the crate's read-a-persistent-handle accessor.
  It hands back a borrowed `&T` there, which is the tag shape this bridge cannot
  have; what comes back here is the handle itself, and an empty handle is
  *undefined* rather than the borrow-of-a-slot-nothing-wrote the crate's own
  `open` gives. The sites use it as `handle.open(scope)`, so the local has to be
  usable directly — which is the shape it now has.
- **`ReturnValue::{set_int32, set_uint32, set_double, set_empty_string}` (8)** —
  the engine has one number kind, so the widths are the crate's way of saying
  which range a value came from; the empty string is a string of length zero and
  not *undefined*, and the test says so both ways.
- **`Function::builder` / `builder_raw` (7)** — the builder existed for
  templates; the `Function` half is what `Function::New` is in the crate (a
  template plus `GetFunction`), so it is three lines over the same machinery.
- **`PromiseResolver::new` (3) and `get_promise` (3)** — and this is where the
  step found a **bug**: `reject` called the *resolving* function, because a
  resolver handle carried only the resolve half, so `reject(scope, error)`
  fulfilled the promise with the error instead of rejecting it. The engine keys
  its record by function identity and the rejecting function's value is nowhere
  else once the capability is built, so the isolate now keeps that value for the
  pair when the resolver is made, and `settle` calls the half it was asked for.
  The test that catches it was written first and failed against the old code —
  which is a better check than a mutation, and the reason this bug was not found
  by the earlier pass that added `resolve`/`reject`.
- **`Isolate::{set_data, get_data}` (3 sites, and two shape corrections)** — they
  took a `usize` and answered `Option<usize>` where the crate takes and answers a
  pointer. That is what `state_ptr as *const JsRuntimeState` (two `E0605`s) and
  the `as *mut c_void` argument (an `E0308`, which had been hidden behind the
  `E0599` that stopped the body being checked) were about; both are gone. The
  receiver stays `&self` where the crate's is `&mut self`, which is the permissive
  direction and the same divergence `ReturnValue`'s setters already have.

Measured: **336 → 292**; `E0599` 243 → 201, `E0605` 2 → 0, `E0308` 78 → 78 — one
appeared behind the `open` sites and went with the `set_data` correction — and the
name count unchanged at 8.

Gates: `cargo test -p v8 --features simdutf` **128 passed / 0 failed** (127 with
the crashing test skipped), `clippy` clean at crate and workspace scope, and
`cargo test --locked --workspace` **5,061 passed / 0 failed / 4 ignored** across
38 binaries with that test skipped. `crates/v8` only, so no sweep is implicated.

**Private names — landed, 347 → 336, and the one divergence they carry.** Eleven
sites were `v8::Private::for_api` (3) and `Object::{get_private, set_private}`
(8), which is how `deno_core` hangs internal bookkeeping on an error object — the
call-site information its `prepareStackTrace` reads.

The engine has no private-name kind, so the bridge models one the way the
language allows: **a private name is a symbol the isolate mints and keeps**, and
a private property is a symbol-keyed property. `IsolateInner` carries the
registry, keyed by the description's code units (the engine's `JsString` has no
hash to be a key), and holding a `Global` so the symbol is a root — the crate
promises a private name is never collected, and a pin is what that is here.
`Private::for_api` is therefore a lookup-then-mint: one description, one name,
for as long as the isolate lives. `get_private` is the symbol-keyed read, with
the crate's empty handle for a name the object does not have — `undefined` is
both "never set" and "set to undefined", so the own-property question decides it,
which is the same extra lookup the crate's own implementation makes. `set_private`
is the symbol-keyed write with throw semantics, answering `Some(false)` for a
rejection and `None` for a proxy trap that threw. And `Data => Private` landed
with them: the check is "the isolate is holding this symbol as a private name",
the same shape as the template tags' — so the deliberately-absent cast list is
ten now, not eleven.

**The divergence, stated and asserted rather than left to be found.** A private
property here *is* a property: `Object.keys`, `for-in`, `JSON.stringify`,
`Object.getOwnPropertyNames` and `in` do not see it (all four asserted), but
`Object.getOwnPropertySymbols` does, and a script that can see the object can
read the value through it. A V8 private name is not a property at all, so that
walk finds nothing. The test asserts the current behaviour on purpose — a host
that hands one of these names to script is handing out something script can find
— and §9 records it as the price of not having a private-name kind in the engine.

Gates: `cargo test -p v8 --features simdutf` **123 passed / 0 failed** (122 with
the crashing test skipped), `clippy` clean at crate and workspace scope, and
`cargo test --locked --workspace` **5,056 passed / 0 failed / 4 ignored** across
38 binaries with that test skipped. `crates/v8` only, so no sweep is implicated.

**The context embedder-data slots — landed, 351 → 347.** Four sites were
`Context::get_aligned_pointer_from_embedder_data` and its setter, which is how
`deno_core` hangs its realm state (`ContextState`, the module map) off a context
through indices it chose. The engine's contexts have no such slots, so the bridge
keeps them: one table on the isolate, keyed by the context's global object — the
identity `payload_eq` already tells two contexts apart by — and the index. A slot
nothing was written to reads back null, which is what the crate answers, and the
test checks that a write lands in the index it was given and nowhere else
(making the lookup ignore the index fails it). The value-carrying pair
(`set_embedder_data`/`get_embedder_data`, which take a `Local<Value>`) is absent:
nothing has asked for it.

Gates: `cargo test -p v8 --features simdutf` **121 passed / 0 failed** (120 with
the crashing test skipped), `clippy` clean at crate and workspace scope, and
`cargo test --locked --workspace` **5,054 passed / 0 failed / 4 ignored** across
38 binaries with that test skipped. No engine crate changed this time, so the
sweep results above stand for the tree as it is.

**The identity hashes — landed, 364 → 351, and the sweeps re-run because an
engine crate changed.** Thirteen `E0599`s were not missing methods at all: they
were `HashSet<Local<'s, Object>>` and `HashMap<Global<Module>, _>`, whose methods
exist but need `Hash` on a handle. The crate's shape is `Hash for Local<'_, T>`
where `T: Hash`, with each tag's hash coming from V8 — an **identity** hash for
every object and for `Name`/`String`/`Symbol`/`Module`, and a **value** hash for
`Value` and the primitives.

This bridge's tags are zero-sized, so there is nothing on a tag to hash; the
impls go on the handles instead, in `data.rs` beside `PartialEq`. The half that
landed is the identity half, over the same identity `payload_eq` already
compares, which is the one contract a host's table needs — handles that compare
equal hash equal:

- Thirty-three tags (`Object` and everything below it, and `Module`) get `Eq`
  and `Hash` for `Local<'_, T>` and `Global<T>`. An object's hash is the engine's
  object id; a function's is the function's id; a module's is the box address.
- `Object::get_identity_hash` and `Module::get_identity_hash` land with the
  crate's names, folded to its non-zero `i32` shape. The module one needed an
  engine addition, because a module record has no identity a language value could
  carry and nothing in `runtime::api` reached its address:
  `api::Module::get_identity_hash` now reports what `PartialEq` already compares
  (the box address, which the arena never moves).
- The **value** tags (`Value`, `Name`, `String`, `Symbol`, the primitives) stay
  absent, and the reason is the equality they would have to agree with: this
  bridge's `Local` equality is the payload's (`f64::eq`, content for strings),
  while the crate's is `SameValue` — so a hash that agrees with ours is a
  different number from the one that agrees with theirs, and for `Number` the
  bridge's equality is not even reflexive (`NaN`). That is a divergence §9 now
  records, and it is the thing to settle before the value half can land.

Measured: **364 → 351**, and `E0599` 271 → 258 — exactly the thirteen bound
failures, checked by diffing the two error lists. Two tests guard the new impls
and both were made to fail: one puts two handles to the global object and one to
a fresh object in a set and a `Global` set, the other does the same with two
compiled modules; forcing the hash to a constant fails them.

Gates: `cargo test -p v8 --features simdutf` **121 passed / 0 failed** (120 with
the crashing test skipped), `clippy` clean at crate and workspace scope, and
`cargo test --locked --workspace` **5,053 passed / 0 failed / 4 ignored** across
38 binaries with that test skipped. `crates/runtime` changed, so the sweep
battery ran rather than being argued away:

| sweep | result |
|---|---|
| test262 `all` (15s/15s) | 48,622 fixtures — 48,461 pass, **0 fail / 0 crash**, 158 skip, **3 hang** (see below) |
| test262 `intl402` | 3,357 fixtures — 3,205 pass, 0 fail / 0 crash / 0 hang, 152 skip |
| wasm core (`run --strict`, 8 suites) | 64,594 checks — 0 fail, 0 pending (20,662 + 7,485 + 105 + 654 + 8,709 + 912 + 77 + 25,990) |
| wasm JS-API (`jsapi`) | 1,001 tests — 0 fail |

The three hangs, reported honestly: `TypedArray/prototype/copyWithin/`
`coerced-values-{start,end}-detached*.js`, reported in two consecutive full
sweeps and in neither the directory sweep (65/65 pass, 0 hang) nor the worker
(they pass alone, in **7.8s, 8.1s and 10.4s**). They are deadline artefacts, not
hangs, and the counts say so: pass plus skip is 48,461 + 158, exactly the
certified total with those three moved from pass to hang, so the engine's verdict
on every fixture is unchanged. Their slowness is inherent to what they build —
with `includeArgFactories: ["immutable"]` the harness still runs the callback
eight times per constructor over a 10,000-element source, and constructing a
10,000-element typed array from the harness's array-like object costs ~105 ms
(574 ms for the `Float64Array` round) in this engine — so the same fixtures sat
close to the deadline at certification and crossed it here. Recorded as an open
item: the construction path from an array-like object is worth a look, and the
next sweep should be read against these three by name rather than by count.

**The documented `runtime` flake reproduced.**
`builtins::function::tests::certified_body_global_read_fast_path_stays_spec_exact`
failed during this pass's first workspace run —
`left: Number(4.0), right: Number(9.0)` at
`crates/runtime/src/builtins/function.rs:1273` — and passed on the re-run. §7 had
recorded it once before as "failed once, never reproduced, unexplained"; it now
has a second occurrence with the values, in a crate this pass cannot reach
(`crates/v8` is in no engine crate's graph).

1. **The value serializer — landed** (13 sites: the two delegate traits, the
   two helper traits, both entry points, and the walk). The design note that
   follows is kept because it is why the walk looks the way it does.
   The *walk* had to be written against the bridge's value API, since the engine
   has no structured clone anywhere (`grep -rn 'structured_clone'` finds none):
   own-key enumeration for objects, elements and holes for arrays,
   `Map`/`Set` through `as_array`, typed-array kind and bytes,
   `Date`/`RegExp` through the agent's own tables, and an identity map for
   cycles and shared references. The *delegate* side was fully specified by
   `deno_core`'s uses and that enumeration held. The *format* is this bridge's
   own version 1, recorded in §9.
   **Slice 1's design.** The walk runs under a scope the serializer and
   deserializer *synthesise* for the call: the delegate traits take a
   `&mut PinScope`, and every accessor the bridge offers takes one too
   (`Local<Array>::get_index`, `Map::as_array`, `Set::as_array`,
   `Object::get_own_property_names`), while the engine offers no scope-free
   equivalent for a `Map`'s or `Set`'s slots. So both entry points store the
   isolate and open a `CallbackScope` + `ContextScope` per call, which is what
   the delegate's error path needs anyway (`throw_data_clone_error` is handed a
   scope). A first cut written against `crux`'s agent-free accessors was deleted
   rather than landed: it invented method names and could not reach a `Map`'s
   entries at all. Both decisions are visible in `crates/v8/serialize.rs` now,
   and the `Global<Context>` that note predicted turned out to be unnecessary
   (the delegate is handed the walk's scope directly, so the stored realm is
   never read).
2. **The wasm tail (now 3 sites + its setter) — surveyed, and two of the three
   need engine work.** `WasmAsyncSuccess` **landed** with the callback vocabulary
   above (an enum, and the async-resolve hook's argument). `WasmStreaming` (2)
   needs the streaming decoder — `crates/wasm` decodes binaries, so what is
   missing is the byte-at-a-time API and the promise it resolves — and
   `CompiledWasmModule` (1) needs a module handle the bridge can hold. Neither is
   a name to declare, and `set_wasm_streaming_callback` waits with them:
   `runtime/setup.rs` is otherwise closed, and its last error is that call.
3. **The stragglers (3 → 1).** `PromiseRejectMessage` (1) **landed**, and it
   turned out to be a name the engine *can* back: the method it was hiding,
   `set_promise_reject_callback`, is now the one isolate callback that fires.
   `NearHeapLimitCallback` (1) **landed** as an accepted-and-not-fired shape.
   `SyntheticModuleEvaluationSteps` (1) is still a callback for synthetic
   modules, which the engine does not have (its JSON/text/bytes modules are
   source-text modules with a synthetic body).
4. **The rest of `v8::Module`** — method-level, so invisible in today's count:
   `get_module_requests` (a `FixedArray` of `ModuleRequest`, which needs a `Data`
   downcast the bridge has no exact predicate for yet), `is_graph_async`,
   `source_offset_to_location` + `Location`, `get_module_namespace_with_phase`,
   `evaluate_for_import_defer`, `create_synthetic_module` +
   `set_synthetic_module_export` (the engine has no synthetic modules at all —
   its JSON/text/bytes modules are source text modules with a synthetic body),
   and `get_unbound_module_script` (no engine equivalent: the engine parses at
   load).

Every one of these is a real subsystem rather than a name to declare: the
inspector protocol, a second GC heap, a task runner, snapshots, structured
clone, stack traces, and cross-thread termination. Two did not fit that shape.
The **resolver** did not need an engine change: the engine resolves imports
itself from what the host has registered, so the bridge satisfies the callback
by walking the graph, asking the host, and registering the answers. The
**platform** turned out to be the other way round — a name whose honest
implementation is "held and never used", because there is no background work to
schedule; that is recorded in §9, not hidden in the code.

## 8. Parked: the C++ face

A working C++ face was built (`v8.h` + `api.cc` + a compat program, all green)
and then removed: it forces a C++ toolchain into this workspace's build and
serves no host we have. It returns when a C++ host actually needs it, generated
if that proves possible.

## 9. Scope decisions

- **Node is out; Deno is the realistic target; Bun is the follow-on.**
- **The one-dependency claim is not real yet.** `crates/slag` cannot be published
  (path dependencies without `version`) and cannot build from a tree without the
  test262 submodule. Either the path deps gain versions and the derived tables
  are pre-generated, or a flattening step is checked in with a CI check.
- **`bitflags` is a bridge dependency now.** `fast_api::Flags` is a bitflags
  type in the crate we stand in for, and the workspace lock already had it
  (transitively), so this adds no download; hand-writing the type would be a
  shape divergence for nothing. Recorded because §11 says no new crate unless
  this plan names one.
- **No facade may swallow failures.** `crates/ffi/src/guard.rs:4-19` turns a
  panic into `R::default()`, which makes an engine bug look like a plausible
  `0`/`false`. The host-facing boundary uses `Maybe`/`MaybeLocal` with a context
  parameter instead, and reserves aborts for invariant violations.
- **Shapes are declared, not implied.** Every shape facade states its tier in its
  header, so nobody mistakes "changes at the class-name level" for "links
  unchanged".
- **The platform is a shape without a producer, and that is stated.** Slag posts
  no task, so `Platform`/`PlatformImpl`/`Task` exist so a host's initialization
  and its own implementation type-check, and the bridge's module docs say so
  plainly. This is the one place where the bridge's tier is "accepted, not
  used"; when the engine grows background work (L3), the platform becomes its
  poster and this decision gets revisited.
- **Flags are recorded, not interpreted.** `V8::set_flags_from_string` keeps the
  string and `V8::get_flags` (the bridge's own accessor) hands it back. The
  engine has no flag surface; a host that needs a flag's effect must not depend
  on it. `set_flags_from_command_line` consumes nothing, so a host sees all of
  its own arguments on the returned list.
- **A snapshot cannot be created, and saying so is the implementation.**
  `create_blob` aborts with the reason; `StartupData::is_valid()` answers
  `false`; `SetDefaultContext`/`AddContext`/`AddContextData` record what a blob
  would carry, so the indices a host stores are real numbers rather than
  invented ones. A blob a host hands *in* is carried and never consumed. The
  alternative — a blob that boots nothing — would move the failure from a place
  a host can see (a loud abort, or `is_valid`) to one it cannot.
- **There is no inspector, and one entry point says so.** `V8Inspector::create`
  aborts with the reason; the data types around it are real. The rejected
  alternatives are worth recording, because both look friendlier and are worse:
  an inert session leaves a debugger hanging with no message, and a protocol that
  errors on every method is indistinguishable from a bug in the host.
  `IsolateHandle`'s termination and interrupt requests answer `false` rather than
  aborting, because they are best-effort requests in the shape they come from
  and because `deno_core`'s exception path calls one on the way out.
- **A setting that is a hint is carried; a setting that is observable is
  implemented.** `FunctionBuilder::side_effect_type` describes what V8's
  optimizer may assume about a call — this engine has no optimizer, so the bridge
  accepts it and says so where a host would look for its effect, and
  `FunctionBuilder::build_fast` lands the same way: the fast-call overloads are a
  compiled entry point this engine cannot enter, so the callback the builder was
  given is what a call reaches. The same test decides `CreateParams::snapshot_blob` (carried, `is_valid` answers `false`) and
  the platform's tasks (accepted, never posted). The distinction matters because
  the two failure modes are not symmetric: a carried hint costs an optimization
  the host never had, while a carried *property* — `length`, say — would be a
  wrong answer.
- **The engine's templates are string-keyed, and the two places that shows are
  stated rather than hidden.** `ObjectTemplate::set` panics on a symbol key — the
  crate's `Template::Set` takes a `Name` and this engine's template properties are
  `JsString`-named, so the alternatives were a silent no-op or a panic — and the
  attributes overload (`set_with_attr`) and `new_instance` are absent until a host
  asks, which the next slice does. `is_onebyte` is the third: the crate's answer
  is a representation hint and this one is exact, which is a refinement of the
  same promise and is documented where a host would read it.
- **The value serializer's wire format is this bridge's own.** The crate we
  stand in for serializes into a format that is internal to V8 — 16 versions of
  history in 2,993 lines of C++ — and matching those bytes buys exactly one
  thing: exchanging blobs with a real V8 process. Every use `deno_core` has is
  in-process, so the bridge defines its own versioned format, states that in the
  module's header, and a host that needs the other thing needs a different
  engine. Recorded here rather than left to be discovered from a stack of
  bytes. **Landed as version 1**: a two-byte header (tag + version) and one tag
  per value kind, little-endian, with object identity written as a reference to
  an earlier id. Legacy streams are *accepted* as the version-0 shape a host can
  ask for and never written, which is what the crate we stand in for does with
  the flag; a stream naming a newer version is refused by `read_header` rather
  than misread.
- **A cast whose predicate the bridge cannot answer honestly is a compile
  error, not a guess.** `impl_try_from!` is 162 of the crate's 173 pairs, and §7
  lists the eleven left out with their reasons (the object template,
  `FixedArray`, `ModuleRequest`/`Private`, the two wasm object types). A check
  that always answers `true` would let a host's failed cast succeed and hand it a
  value of the wrong shape, which is worse than a name the compiler reports; the
  `TagCheck` doc comment in `crates/v8/data.rs` says so where the next reader
  meets it. The template predicate is the one case that needs state rather than a
  payload test — it asks the entered realm's isolate whether that address is one
  of its templates — so a cast with no realm entered fails, which is the
  documented behaviour of every other table predicate in the bridge.
- **A receiver the crate has and this bridge does not is a bound the host's call
  site fails on, and the bridge grows it from demand.** `NewTryCatch` has three
  receivers here and five there (`crates/v8/scope.rs`); the two absent ones — an
  escapable handle scope and a nested try-catch — are absent because no host has
  asked, and the compiler names the missing impl the moment one does. The
  `ContextScope` receiver also needed `ContextScope::new` to hand back the scope
  rather than its storage, which is what the crate does: a shape that looks
  equivalent can decide whether an impl is reachable through `DerefMut` at all.
- **Handle equality here is the payload's, where the crate's is `SameValue`.**
  `Local<Number>` under this bridge compares with `f64::eq`, so `NaN != NaN` and
  `-0 == 0`; the crate compares with `v8::Value::SameValue`, so `NaN == NaN` and
  `-0 != 0`. Nothing host-facing has asked yet, and the consequence today is one
  missing group of impls rather than a wrong answer: the value tags cannot carry
  a `Hash` that agrees with this bridge's `==` and with the crate's at the same
  time, which is why only the identity half of the hash surface landed. Settling
  it means asking the engine for the same operation the crate asks V8 for
  (`same_value` already does, for the explicit call), and then the value hashes
  follow. Recorded here rather than left to be discovered from a `HashMap` that
  behaves differently on two NaNs.
- **A private name is a symbol here, so it is a property, and that is a
  divergence.** The engine has no private-name kind, so `Private::for_api` maps a
  description to a symbol the isolate mints and keeps, and a private property is
  a symbol-keyed property. The walks a private name has to be invisible to do
  not see it (`Object.keys`, `for-in`, `JSON.stringify`,
  `Object.getOwnPropertyNames`, `in` — all asserted in `crates/v8/private.rs`),
  but `Object.getOwnPropertySymbols` does, and the value is reachable through the
  symbol it returns; a V8 private name is not a property at all and cannot be
  found that way. A host that hands one of these names to script is therefore
  handing out something script can find on any object it shares. The alternative
  — private names as a kind the engine keeps off the properties entirely — is
  engine work rather than bridge work, and is the thing to do if a host ever
  needs V8's strength here; recorded now so the weakness is a decision rather
  than a surprise.
- **The tag shape is the operator's call, and it landed as the crate we stand in
  for has it.** `deno_core` calls methods on `&v8::Value` and `&v8::String` and
  transmutes between them (`libs/core/runtime/ops.rs:273`, `convert.rs:211`),
  which needs `Local<'s, T>: Deref<Target = T>` over the tags. Rust allows one
  `Deref` per type, so the deref chain could not stay on `Local`: it is now
  `Target = T`, and the tags deref into `LocalHandle<'s, T>`, which carries the
  methods and the inheritance edges until they move onto the tags themselves.
  **The tier this states:** reachability is the crate's (`&v8::Value` calls what a
  `v8::Local<v8::Value>` calls, and the reinterpretations the host writes work);
  what is not yet is UFCS on a tag and a trait implemented *for* a tag, and both
  follow from the same per-file moves. Entry price and payoff, measured: 209 → 62
  `deno_core` errors, with the 78 `E0308`s gone entirely.
- **A pre-existing crash in the v8 test binary is recorded, not hidden.**
  `function::tests::the_data_a_built_function_carries_survives_a_collection`
  aborts the process on most full-binary runs (it passes in isolation, so the
  interaction is with earlier tests in the same process). It predates the
  serializer and is not fixed here; §7 carries the evidence.
- **The engine fires one of the isolate-level callbacks, and each of the others
  says so where a host would look for its effect.** `set_promise_reject_callback`
  is implemented over `HostHooks::promise_rejection_tracker`, which is the engine
  reporting exactly the two events a rejection callback can act on. The rest —
  prepare-stack-trace, `import.meta`, dynamic import, wasm async resolve,
  near-heap-limit, `set_idle`, capture-stack-trace — are accepted and recorded as
  not fired, because the engine has no such event: it builds error stacks itself,
  builds `import.meta` at load, resolves a dynamic import from the modules the
  host registered, compiles wasm synchronously, and neither enforces a heap limit
  nor profiles. The rejected alternative is the one the inspector note already
  gives: a callback that is never called and never says so is indistinguishable
  from a bridge bug. So each carries a doc comment naming what the engine does
  instead, and the two things a host *can* act on — refusing to compile, or a
  wrong answer — are both avoided: this is the "carried hint" side of the line,
  not the "carried property" side.
- **A rejection message carries values, not a pointer.** `PromiseRejectMessage`
  is `[usize; 3]` in the crate we stand in for — a pointer to V8's own struct —
  and here it holds the engine's promise, reason and event. `get_promise`,
  `get_event` and `get_value` answer the same three things, and the callback scope
  deno opens from `&message` resolves through the same `NewCallbackScope` the
  other context-less callbacks use. The crate's two extra events
  (`PromiseRejectAfterResolved`, `PromiseResolveAfterResolved`) exist in the enum
  and cannot arrive: the engine has no event for them.
- **The prepare-stack-trace callback is typed as the host's function, not as the
  C pointer.** There, the value comes back through the platform's calling
  convention — a hidden return pointer on Windows, a register elsewhere — which
  exists only so a `MaybeLocal` can cross C. Nothing crosses C here, so the type
  is the host function's shape and the `MapFnTo` bound is the one the crate's own
  call sites satisfy (`for<'a> Fn(&mut PinScope<'s, 'a>, …)`).
- **Claiming the host-hook seam is a decision, not a side effect.** The seam is a
  single trait per isolate, so installing a rejection callback replaces whatever
  was there. Nothing else in this bridge sets it, and an isolate whose host never
  calls `set_promise_reject_callback` keeps the defaults — but a later hook the
  engine grows joins `PromiseRejectHooks` rather than reaching for the seam
  again, and that is why the type exists rather than an inline `Box`.
- **The symbol surface is the engine's own registry, and what is absent is absent
  by demand.** `Symbol::for_key` reads `Agent::global_symbol_registry`, the list
  `Symbol.for` reads, so a host-made name and a script-made one are one symbol;
  the well-known accessors read `crux::symbol::well_known`, which is what the
  engine installs `Symbol.name` from, so a host never mints a look-alike. The
  deliberate omission is `Symbol::for_api`: the crate we stand in for gives it a
  *second* registry JavaScript cannot reach, the bridge has no such table, and
  offering the name over the one registry would be a promise it could not keep —
  a script could then find an API symbol. Recorded with `Symbol::new` and
  `Symbol::description`, which are absent until a call site asks.
- **A primitive array is a JavaScript array here, and the divergence is smaller
  than it looks.** V8's `PrimitiveArray` is a C++ heap object that is itself an
  `Array` subclass, so writing one as the array the bridge builds for the host is
  close in behaviour and exact in shape: a length, integer-indexed writes, and
  reads that answer *undefined* for a slot nothing wrote. Its slots start
  *undefined* rather than uninitialized, which a host cannot tell apart either
  way, and the value is never a property of anything, so the walks that could see
  it do not.
- **A key walk's object list is the whole difference between the two enumeration
  calls, and a filter's two halves behave differently.** `get_property_names` and
  `get_own_property_names` share one walk over a list of objects, because the
  difference between them is which objects are in that list. The index filter had
  been ignored by both — a host asking for `SkipIndices` got the indices back —
  and the shadowing rules came from V8's `KeyAccumulator`: a key the *attribute*
  filter rejects still hides the same key further up the chain, while a key
  dropped by the kind or index filter does not.
- **A receiver the crate puts on the scope is a borrow-shape decision, not a
  naming one.** The continuation-preserved value was a method on the bridge's
  isolate where the crate keeps it on the handle scope, and the cost was paid in
  the *host*: a handle tied to the borrow of a scope cannot outlive a `&mut` use
  of that scope, which is precisely the sequence a suspension performs. The pair
  moved onto the scope (the isolate still stores the value) and the getter hands
  back a handle carrying the scope's lifetime.
- **A store built from a host's bytes holds a copy, and the widths are the part
  that matters.** There, `new_backing_store_from_bytes` takes the buffer by value
  and V8 reads it where it is; here the bytes are read into an engine block, so the
  host's allocation is freed as the call returns. The length and the contents —
  everything a host can observe except the address `data()` answers with — are the
  same, and the `Rawable` widths (`Box<[u16]>` is two bytes an element) are what
  makes the difference invisible to script: a bridge that used the element count
  would hand over half the bytes.
- **The engine's two shared-array-buffer constructors do not agree, and the bridge
  compensates rather than changing either.** `shared_array_buffer_from_block` (the
  host-facing one) records sharing on the *buffer*, while the language-facing
  constructor also calls `SharedBuffer::mark_shared` so the block's own flag
  agrees. The bridge's `SharedArrayBuffer::with_backing_store` calls it for that
  reason. Left as an open decision (§12) because the host-facing function has three
  other callers — wasm memory, workers, and the test262 runner — and a change to it
  is a change to their behaviour.
- **A store over the host's own memory is the shape a copy cannot stand in for,
  so the engine grew it.** `ArrayBuffer::new_backing_store_from_ptr` is the
  aliasing constructor: a host hands over a pointer and a C deleter, and the
  point is that a script writing through a buffer over the store writes the
  host's bytes. `new_backing_store_from_bytes` could be faked by reading the
  bytes in, and the only cost was an address a host could not see; this one
  cannot, because the host's allocation *is* the store. `SharedBuffer::borrowed`
  is that block, and the decisions inside it are two. It is one shape in both
  builds, with *plain* atomic operations rather than the machine atomics an owned
  block gets under `workers` (the host's bytes are not a word array, and a host
  must not share one between agents — stated in the constructor's `# Safety`
  section), and it is declared `Send`/`Sync` under `workers` by assertion,
  because a block travels to a worker thread and what a host passes (a C
  function pointer plus its registration data) is `Send` in fact but cannot say
  so to the compiler. The alternative — keeping the borrowed field behind
  `cfg(not(feature = "workers"))` — was tried and is why this is recorded: it
  compiled in isolation and left the workspace not type-checking, because
  `crates/test262` turns `workers` on for the whole unified graph and
  `crates/v8` then asked for a constructor that same build had compiled out.
- **The cppgc heap allocates and never collects.** The tier is stated in
  `crates/v8/cppgc.rs`'s header and in §7: real allocation, real handles, real
  tracing, real reclamation at the heap's end — and no collection before it,
  because a sweep without stack scanning can free an object a host's stack
  pointer still names. The alternative tiers were considered and rejected: a
  heap that refuses (the inspector's treatment) would take WebCrypto and canvas
  away from a host that can otherwise work, and a heap that sweeps anyway would
  be unsound. A host that needs reclamation needs L2.

## 10. Build order

Engine side: (1) L1 roots — done; (2) platform + task runner; (3) snapshot +
external references + per-isolate/context data slots; (4) module resolver as a
host trait, unbound scripts, code cache, script origins. Then, in the order the
shim histogram implies: structured frames and termination, host memory (partly
landed — a store over memory the host owns is `SharedBuffer::borrowed`; the
accounting half, externally allocated memory and backing-store shrink, is not),
inspector and source maps, traced host objects, structured clone.

Bridge side: (1) signature-compatible Rust face — done, `serde_v8` type-checks;
(2) grow the surface from the items `deno_core` names, in call order — the name
frontier is nearly closed (4 left: `WasmStreaming` twice, `CompiledWasmModule` and
`SyntheticModuleEvaluationSteps`) and the serializer is landed, so this
is a **method-level** stage: 617 type errors were visible for the first time, the
first pass through them took it to 374, the cast closure to 369, the scope and
property bounds to 364, the identity hashes to 351, the embedder-data slots to
347, private names to 336, the method tail to 292, scheduling and exception
control to 260, the isolate-level callback vocabulary to 242, the symbol surface
to 233, the primitive array to 225, the leftovers to 217, the buffer-handing
shapes to 213, the host-memory store to 209, the tag shape to 62, the template
surface to 56, and what is left
is the method surface (47 `E0599`s, all of them named methods and subsystems: the
message and stack-trace surface, synthetic modules, wasm streaming, code cache,
source offsets, the extras binding object, and the half of the template cluster
that needs attributes, static properties and `new_instance`), the 4 names, and
four stragglers (3 `E0515`, 1 `E0282`). Three
surveyed-and-left items sit outside those counts' reach — `get_constructor_name`
(needs V8's map), `get_extras_binding_object` (needs an engine-side extras
object) and `get_heap_statistics` (needs byte accounting in `crux::heap`) — and
everything else needs the tag shape or an engine capability; with the shape
landed, the tag-shape column is closed — the remaining 53 are methods to write
and the subsystems they name; and the shape's own tail is (a) the methods moving
from `LocalHandle` onto the tags, file by file, then (b) deleting `LocalHandle`
and its deref table, which is when the tier §9 states stops being a tier;
(3) point the local `deno/` checkout at the crate and run a script — blocked on
those type errors, and on the runtime gaps this work found (`queueMicrotask`, and
a host that must boot without a snapshot); (4) migrate, then
delete.

## 11. Working rules

- Tracked files only; nothing in gitignored or hidden paths.
- One file per change, with the diff pasted into the reply.
- No vendored third-party source; no new crate unless this plan names it.
- `crux` / `runtime` edits are named before they are made.
- After each change: the focused test, then
  `cargo clippy --locked -p <crate> --all-targets -- -D warnings`.
- Git belongs to the operator: never `add`, `commit`, `checkout`, `revert`.

## 12. Open decisions

1. **Weak persistent handles** — the last L2 item. Design sketched in
   `.notes/host-object-gc.md` §4.3; not started.
2. **Snapshot format** — ours to version; external references must stay
   index-stable across builds, a compatibility surface from day one.
3. **Sealing `slag::api`** — the re-export exists (`crates/slag/src/lib.rs`, with a
   test that drives a module through it), so a host can depend on `slag` alone.
   Still open: `Local::value`, `Isolate::agent`, `Local::as_object` name
   `crux`/`runtime` types that are reachable but not yet promised, and `Module`
   hands out the engine's status type rather than one of its own (deliberate
   while the vocabulary is identical).
4. **`jsc`'s fate** — kept for now (§2); whether it grows the missing typed-array
   predicates or is left alone is undecided.
5. **`Array::new` ignores its length** — found while building the primitive array,
   recorded rather than fixed because it changes observable behaviour of an
   existing method and no error catches it. The bridge's `Array::new(scope,
   length)` documents "a new array of `length` holes" and builds an empty one, so
   a host that asks for three elements and reads `length` gets zero;
   `deno_core` writes every slot it asked for, which is why it never notices. The
   fix is a holey array of that length — `length` is a property the engine
   writes — and it needs a decision, not a drive-by.
6. **`shared_array_buffer_from_block` records sharing on the buffer and not on the
   block.** The language-facing constructor in the same file does both
   (`SharedBuffer::mark_shared`), so the two disagree about what a store built over
   one of these blocks reports. The bridge compensates where it builds a shared
   buffer; the engine fix is one line but touches wasm memory, workers and the
   test262 runner, so it is the operator's call rather than a drive-by.
