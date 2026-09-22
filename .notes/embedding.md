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
| **L3** | own scheduling, GC coordination, threads | platform/task runner, microtask policy, snapshots, external references, termination, host memory | **partly landed** — the **snapshot format's v1 landed** (§7's last seven records, ledger item 16): a versioned blob over the host's attached context data, one slot per context with each slot written and read against its own realm, restored into a rebuilt realm, **external references landed with it** — a host pointer is an index into the table the host rebuilds for every load, which is the compatibility surface this row always said it was — **a JavaScript function is carried as the source it is re-parsed from**, **a bind as its target and bound state**, **a class constructor as the class text the engine's own class evaluation re-runs**, **an arrow as the expression it was written as**, **a host callback as its table entry plus the data it reads**, **a module record — which is not a language value — as the name and source the engine's own compile path rebuilds it from, so a slot's items come back at the indices the host attached them under**, and **the realm's own global functions, the members of its own builtin objects by the spec's names and a method that reads a private name, as a member of its class**, so a host's attached callbacks and the builtins its graph reaches come back callable, and **a blob now loads as well as builds** — `read_snapshot` was exercised against deno's blob for the first time and now reads through every value the walk carries and every item its slots hold (§7); what a restore cannot rebuild is the scope a closure closed over, which the format states rather than implies, and what a host's own `FromSnapshot` path additionally expects — its realm's bootstrapped global — was **measured**: a bootstrapped `globalThis` walks, restores into a fresh isolate and its bootstrap functions run (§7's last two records, §12 item 11), so the remaining work there is the load applying it rather than a gap in what can be carried. What remains on this row: the task runner, termination, and the host-memory accounting half. `MicrotasksPolicy` genuinely landed 2026-09-21 (`Explicit` leaves the queues to the host, `Auto` drains at the outermost entry, depth-guarded so a callback cannot have jobs run under it). Until then this row said "`MicrotasksPolicy` only" while no policy existed anywhere in the tree — the same misstatement the L1 row carried, caught the same way, by grepping for the type rather than trusting the row |

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
- **A snapshot blob is carried, not consumed, when it is not one this engine
  wrote** — and `StartupData::is_valid()` answers that question for real now
  (superseded by §7's last record: the engine has a format, and a blob of it is
  consumed by the isolate it is handed to). Before the format landed, `is_valid`
  answered `false` for everything, because a blob from V8 is not valid for this
  engine and Slag produced none of its own. A host that depends on its
  snapshot's contents must still be able to be built without one — which is the
  configuration a Deno-class host has.

Flags are recorded (`V8::get_flags`, the bridge's own accessor) and not
interpreted: Slag's engine is not flag-configurable, so a host whose behavior
depends on a flag gets that flag's absence rather than a wrong one. Two gaps
this surfaced, both engine-side and both recorded here rather than worked
around: **`globalThis.queueMicrotask` does not exist in the engine** (Deno's
core JS uses it, and its `--enable-queue-microtask` flag is how V8 supplies one),
and there is no engine flag surface at all.

**Snapshot creation — landed, and it refuses.** *(Superseded by §7's last record:
this bridge now produces a blob. What follows is the state this step landed in,
kept because the refusal decision it records is what the format then replaced.)*
`Isolate::snapshot_creator` /
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
the *second* read of an index; here it was every read, which was the same
statement about a snapshot that was never made — so the restore path reports
through the error channel the shape already has rather than panicking. With the
format landed the first of the two reads a restored context's items once, and
the second is what still answers `NoData`; the isolate-level one answers it for
every read, because the format carries a *context's* data and that is where a
host attaches it.

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
| after the attribute-carrying half of the template cluster (`PropertyAttributes` in the engine, `ObjectTemplate::{set_with_attr, set_accessor_property, new_instance}`) | **52** (see below) |
| after the static half of the template cluster (`FunctionTemplate::{set, inherit}`, the per-realm materialization memo, and `set_internal_field_count`) | **49** (see below) |
| after the three stragglers a `Context` and an `Object` answer (`get_constructor_name`, `get_extras_binding_object`, `Context::from_snapshot`) | **44** (see below) |
| after the message surface (`Exception::create_message`, `Message::{get, get_script_resource_name, get_line_number, get_start_column, get_stack_trace}`, and the recorded position behind them) | **37** (see below) |
| after the module structure surface (a module's requests and the four reads on one, an offset's location, the graph's async-ness, the deferred namespace, and the two callback-scope sites `Local::new` was blocking) | **25** (see below) |
| after the synthetic-module surface (the record kind the engine gained, its export writes, its evaluation steps, and the `SyntheticModuleEvaluationSteps` re-export) | **21** (see below) |
| after the callback scope's lifetime (the two `E0515`s) | **19** (see below) |
| after the compiled-module surface (the engine's wasm API exposed: `api::WasmModuleObject`, `api::CompiledWasmModule`, and the bridge's `WasmModuleObject` over them) | **15** (see below) |
| after the streaming half (the engine's `HostHooks` wasm-streaming pair, `api::WasmStreaming`, and the bridge's `Isolate::set_wasm_streaming_callback` / `WasmStreaming`) | **12** (see below) |
| after the unbound scripts and the code cache (`UnboundScript`/`UnboundModuleScript`, `create_code_cache` on the script, the module script and a function, `get_source_mapping_url`) | **6** (see below) |
| after the stack-trace frames (`StackTrace::current_stack_trace` and the frame accessors, over the engine's execution contexts) | **4** (see below) |
| after the escapable handle scope (the macro's inference, which the `E0282` that had been unexplained since the serializer step turned out to be) | **3** (see below) |
| after the heap statistics (`HeapStatistics`, the four accessors a host reads, over the arena's own numbers and the agent's own record of live buffers) | **2** (see below) |
| after the module-graph tail (`Module::{evaluate_for_import_defer, get_stalled_top_level_await_message}`, over the engine's own defer machinery and a walk it already had) | **0** (see below) |

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

That was the shape of the frontier then. It is no longer: with no names left, the
count became a *method-level* metric, and it closed at 0 — the last steps are two
`E0599`s and then none, with each step's own record below naming which code
classes moved rather than only the total. A count of 0 does not mean `deno_core`
works: it means it *compiles* against this crate, which is all that metric ever
measured, and §7's last record and §10's (3) say what is left.

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

**The message and stack-trace surface — surveyed, not landed, and the estimate for it
changes.** Ten sites: `Exception::create_message` (3: `error.rs:1035`,
`inspector.rs:981`, `bindings.rs:1635`), `Message::{get, get_script_resource_name,
get_line_number, get_start_column}` (4: `error.rs:841/844/846/975`) and
`StackTrace::{current_stack_trace, get_frame_count, get_frame}` with
`StackFrame::{is_user_javascript, get_script_name}` (2: `import_graph.rs:164`,
`ops_builtin_v8.rs:1518`). Every one of them asks the engine for a *position*, and
this is what the survey found where the bridge would need it:

- **The engine's frames carry a function name and nothing else.**
  `builtins/error.rs::define_stack` walks `agent.execution_context_stack` at error
  construction and stores a **string** per error object (`    at {function}`),
  which is what `error_stack` holds and what the `stack` accessor serves. No line,
  no column, no script name.
- **There is no per-activation source position to ask for.** `ExecutionContext`
  (spec 9.4) has `function`, `realm`, `script_or_module`, the three environments
  and `source`, and no span; spans live in the IR and appear in `JsError.span` and
  `capture_source(agent, span)` — read from *static* nodes (`function.span`,
  `expr.span`), not from “where execution is now”. So `Message::get_line_number`
  and `StackFrame::{get_line_number, get_column}` have nothing to answer from, for
  a runtime error or for a frame.
- **`is_user_javascript` needs a notion of script *type*.** V8's answers from the
  script a frame belongs to (an ordinary script is user JavaScript, an
  engine/embedder one is not), and `deno_core` uses it to skip its own
  `ext:core/01_core.js` frames; the engine does not classify scripts yet.

So this is the **structured frames** item the engine-side order already lists, not
ten methods: per-activation source positions (updated where the IR knows a span),
script classification, and then the frame surface. Landing it well is an engine
subsystem with its own acceptance tests, and starting it at the end of a session
is how a half-landed thing gets written, so it is recorded here instead — in two
pieces, because only one of them needs the positions:

1. **`StackTrace`, without line or column.** An engine accessor over the
execution-context stack that already exists — a frame as `{ function name, script
name, is_user_javascript }` — closes `import_graph.rs:164` and
`ops_builtin_v8.rs:1518`, both of which read exactly those three and never a
number. `StackFrame::{get_line_number, get_column}` stay **absent** rather than
answering `0`: the crate's callers get a compile error, which is the honest
channel, and `is_user_javascript` needs the script classification above before it
means anything.
2. **`Message`, from the locations the bridge itself knows.** A compile-time
failure has a span (`JsError.span`) and the source text in hand at compile time,
so the bridge can record `{ script name, line, column }` per error object and
answer `Message`'s four methods from it. A *runtime* error has no recorded
location, and the crate's own shapes are `Option`s — `get_script_resource_name`
and `get_line_number` answer `None`, which is what `deno_core`'s
`JsStackFrame::from_v8_message` already handles (`?` on both). What stays absent is
the runtime location and any line/column in a frame, which is the position work
above. This piece is bridge-only and closes 7 of the 10.

Both pieces are ready to start and named here so the next session does not
re-derive them. Piece 2 is now landed (below); piece 1 is not started, and it needs
the script classification to be honest about its one boolean.

**The three stragglers a `Context` and an `Object` answer — landed, 49 → 44, all bridge.**
Five sites: `Object::get_constructor_name` (`ops_builtin_v8.rs:1342`),
`Context::get_extras_binding_object` (`bindings.rs:372`, `ops_builtin_v8.rs:1607`)
and `Context::from_snapshot` (`jsruntime.rs:2844`, `:2845`). No engine change: the
first two had the pieces already and the third is a refusal.

- **`get_constructor_name` reads the chain.** The crate answers from the object's
  *map* — the constructor it was instantiated with — with `Symbol.toStringTag` as
  the other source and the string "Object" as the fallback. Here the prototype
  chain is walked for the nearest own `constructor` whose `name` is neither empty
  nor "Object", then "Object" — V8's own helper skips both of those and keeps
  walking, so the cases line up: `[]` → "Array", `new Map()` → "Map", a class
  instance → the class name, `Object.create(null)` → "Object". Two divergences,
  both recorded in §9: a map remembers the constructor an object was *made* with
  while this reads the property as it is now, and `Symbol.toStringTag` is not
  consulted because the property reads this bridge can make are name-keyed. The
  second is visible in the test, and it is smaller than it looked:
  `new Map().entries()` answers "Iterator" — the name the engine's iterator
  prototypes carry — where V8's tag answers "Map Iterator".
- **The extras binding object is a per-context object the isolate owns.** There,
  V8 makes one per context and fills it from the embedder's snapshot (the host
  reads `console` out of it); here it is created on the first ask and left empty,
  because this bridge cannot run a snapshot — so what the host's bootstrap puts in
  it is what it holds, and the host is the one that knows. It is the same object on
  every ask, keyed by the context identity the embedder-data slots already use and
  pinned like a template.
- **`from_snapshot` is `None`, and the tier is the entry point.** The crate's
  `Option` is the channel, and this bridge cannot create a snapshot (§9's
  `create_blob` note says why), so there is no blob whose contexts could be
  restored: a host that booted without one takes its own other branch, and one
  that asks is told rather than handed a context that is not the one its blob
  names.

Measured: **49 → 44**, `E0599` 40 → 35. Three new tests — the name table above
(including the iterator divergence, asserted on purpose), the extras object being
one object per context that starts empty and keeps what the host puts there, and
`from_snapshot` answering `None`. Both test expectations were wrong first time and
the engine was right: the iterator case answers "Iterator" rather than "Object",
and an empty object's absent name reads back *undefined* rather than as no read at
all.

**The message surface, piece 2 of the split above — landed, 44 → 37, one engine
exposure.** The seven sites that need a `Message` and no frame: `create_message`
(`error.rs:1035`, `inspector.rs:981`, `bindings.rs:1635`) and
`Message::{get, get_script_resource_name, get_line_number, get_start_column}`
(`error.rs:841/844/846/975`). `Message::get_stack_trace` came with them because
`inspector.rs:982` asks for it the moment the line above compiles.

What the answers are made of:

- **`create_message` copies nothing.** V8 builds a record at the throw site and
  the message answers from it; here the handle names the exception itself, and
  every answer is read from it. That is why `Message` is a tag outside the `Data`
  hierarchy whose payload is an ordinary value handle.
- **The position is the bridge's, recorded where the error is thrown.**
  `crates/v8/position.rs` holds a `Position { name, line, column }`, and the
  isolate keeps one per error object keyed by identity — the way the engine keys
  its own per-object tables, and the way V8 keys the same record (`error_start_pos_symbol`
  and friends, read back by `ComputeLocationFromException`,
  `v8/src/execution/isolate.cc:3640`). Line 1-based and column 0-based are V8's
  own conventions (`JSMessageObject::GetLineNumber`/`GetColumnNumber`,
  `v8/src/objects/js-objects.cc:6093`, the latter's comment being *no '+1' in
  contrast*), the offsets are added by `Script::AddPositionInfoOffset`'s rule
  (`script.cc:324`: the line always, the column only on the first line), and the
  spans counted are UTF-16 code units, which is what the parser's spans are and
  what a line's terminators are (`\n`, `\r` not followed by `\n`, `\r\n`,
  U+2028, U+2029 as one sequence each).
- **Only a failed compile records one, and that is the honest half.** A `JsError`
  span indexes whichever source the failing code was parsed from, so an error
  raised while a script runs can carry a span into any other script; recording it
  against the running one would put a wrong position on an error. At a compile the
  error necessarily came from the text in hand. `CompileFunction` records nothing
  for the same reason in reverse: its body is wrapped before it is parsed, so the
  span names a line the host never wrote. Those are the only three compile paths
  (`Script::compile`, `script_compiler::compile`, `compile_module2`), and the
  origin — which the engine never sees — is where the name and the two offsets
  come from.
- **The text is V8's uncaught rendering.** The message `create_message` makes
  holds the `Uncaught %` template (`v8/src/common/message-template.h:25`) over a
  side-effect-free string of the exception (`MessageHandler::GetMessage`,
  `messages.cc:188`), so `Message::get` renders `name: message` for an error
  (V8's `NoSideEffectsErrorToString`, `objects.cc:536`) and the built-in tag —
  `[object Array]`, `[object Function]` — for anything else
  (`NoSideEffectsToString`, `objects.cc:719`). Two property reads away from V8:
  V8 reads `name`/`message` as *data* properties so no host code runs while an
  error is formatted, and this reads them with a [[Get]]; and V8's `#<CtorName>`
  form for an object whose `toString` is the default is not reproduced.
- **`get_stack_trace` is `None`, and that is V8's default too.** V8 captures a
  trace for a message only when the embedder asked
  (`SetCaptureStackTraceForUncaughtExceptions`, which this bridge accepts and
  carries nowhere); the one caller in `deno_core` hands the answer to an
  inspector that refuses. The frame surface itself is piece 1, and the two
  `StackTrace::current_stack_trace` sites are still errors.

**One engine exposure, named before it was made:** `runtime::api::Object::builtin_tag`
(a thin wrapper over `builtins::object::builtin_tag`, which became `pub`), because
the `[object Tag]` an object is described by comes from the engine's own brands —
[[Call]], [[ParameterMap]], the boxed-primitive marker, the error/Date/RegExp
slots, the object kind — and none of them is reachable by a property read. It is
"already existing infrastructure", which is the tier §9 sets for an engine change.

Measured: **44 → 37** (`E0599` 35 → 28). Twelve new tests, each verified by
mutating the code it guards and watching it fail: five over `line_and_column`
(the line/column count, every line terminator, the code-unit count, an offset past
the end, the origin offsets and their saturation) and seven over the message — a
compile error answering its recorded name/line/column, the position following the
source, the offsets shifting it, a position outliving the scope it was recorded
in, a message with no record answering `None` for all three, the uncaught
rendering across eleven value shapes, and an error's text coming from its own
`name`/`message`. One test expectation was wrong first time:
`new Error('m').message = 42` as a *source* evaluates to `42`, so the case had to
build the error and return it.

Gates for the slice: `cargo test -p v8 --features simdutf` (176 in that binary, 12
of them new), `cargo test --locked --workspace -- --skip
the_data_a_built_function_carries_survives_a_collection` — **5,110 passed / 0
failed / 4 ignored across 38 binaries**, the 12 over the 5,098 recorded before it
— and `cargo clippy --locked --workspace --all-targets -- -D warnings` clean. No
sweep: `crates/runtime` changed, and the change is one new method on
`api::Object` plus a `pub` on the `builtin_tag` it wraps, which is a visibility
edit and cannot move behaviour. Checked rather than assumed, the same way §7 does
it for the microtask change: no runner names `runtime::api` at all (`grep` over
`test262`, `wasmtest`, `wasm`, `cli` — only `crates/slag/src/lib.rs` re-exports
it), and nothing outside the three files that define and use it names
`builtin_tag`. The corpus totals recorded in §5 therefore still stand for this
tree.

**The module structure surface — landed, 37 → 25, and two of the three callback-scope
sites with it.** Twelve sites: `Module::get_module_requests` (`map.rs:965`), the
four reads on one request — `get_specifier` (`:976`), `get_import_attributes`
(`:979`), `get_source_offset` (`:1048`, `:995`), `get_phase` (`:1063`) —
`Module::source_offset_to_location` (`:995`), `Module::is_graph_async`
(`evaluation.rs:261`, `ops_builtin.rs:768`),
`Module::get_module_namespace_with_phase` (`dynamic.rs:616`), and two of the three
`E0515`s (`map.rs:1435`, `ops_builtin.rs:705`).

- **A request is a payload, not an object.** V8 has a `v8::ModuleRequest` heap
  object inside a `FixedArray`; the engine keeps a module's requests in the
  module's own record, so `Payload::ModuleRequest { module, index }` names one and
  `Payload::ModuleRequests { module }` names the list, which is what
  `Module::get_module_requests` hands back under the `FixedArray` tag. That is
  why `FixedArray::{length, get}` dispatch on the payload: the attributes array a
  resolve callback receives is still the engine array it was, and a request list
  is the record. The payoff is the cast: `TagCheck for ModuleRequest` is the
  payload's own, where the alternative — building the requests as engine arrays
  and casting by shape — would accept any host array that happened to look like
  one. The divergence is that a request's reads are one field of the engine's
  record away rather than a heap object's, which no host can see.
- **The graph's async-ness is V8's own walk.** `Module::IsGraphAsync`
  (`v8/src/objects/module.cc:614`) is a worklist from the root over the modules
  its requests reached, stopping at the first `has_toplevel_await`. The engine
  had only the per-module `module_has_tla`, so
  `runtime::module::module_graph_has_tla` is that walk over the realm's own link
  table. A request whose specifier nothing registered is not walked, which is
  what an unlinked request is — and it makes the answer *before* linking “the
  root alone”, which the test pins.
- **`source_offset_to_location` is the engine's `SourceText`.** `SourceText::
  line_column` (`crates/syntax/src/source.rs`, with its own tests) already
  counts line terminators the way `Script::GetPositionInfo` does, so the api
  method is that call and the bridge's `Location` is the 1-based-to-0-based
  conversion — V8's `Location` is 0-based in both numbers (`api.cc:2347` builds
  it from `PositionInfo`, and `deno_core` adds one to report it).
- **The deferred namespace was already in the engine.** `module::deferred_namespace`
  and the `[[DeferredNamespace]]` slot exist for import-defer, so
  `get_module_namespace_with_phase` is the choice between the two namespaces V8
  keeps, and the test pins that they are different objects. V8's own `DCHECK`
  (`module.cc:340`) admits only evaluation and defer; its release fallthrough for
  anything else is the deferred one, which is what a source phase gets here too.
- **`Local::new`'s scope binding was the `E0515`.** A host's resolvers open a
  callback scope and return a handle made inside it (three sites); the crate
  accepts that because a `Local` there is a pointer whose lifetime is *phantom* —
  `NonNull<T>` plus `PhantomData<&'s ()>`, checked in `scratch/ref/v8-150/handle.rs:109`
  — while this bridge's `Local::new` tied the answer to the scope's *first*
  lifetime, which for a callback scope is the borrow of its storage. Freeing it
  (`&PinScope<'_, 'i, ()>`) is honest rather than convenient: a handle here is the
  value it carries, so nothing about it depends on a scope's borrow, and the new
  test is the host's own shape — a callback that returns a handle made in its
  scope, which does not compile without the change.

**One engine exposure and one engine function, both named before they were made:**
`api::Module::{request_count, request, is_graph_async, source_offset_to_location,
deferred_namespace}` — thin over what the record already had — and
`runtime::module::module_graph_has_tla`, the walk above.

Measured: **37 → 25** (`E0599` 28 → 18, `E0515` 3 → 1). Six new tests, each
verified by mutating the code it guards and watching it fail: the three reads on a
request in source order (indexing the list wrong fails two of them), an offset's
location (dropping the 0-based conversion fails it), the two elements of `FixedArray`
(the cast's honesty, which `TagCheck → true` breaks), the graph walk (not enqueueing
what a request reached fails it), the two namespaces (answering the eager one for
defer fails it), and the callback-scope shape (which fails to *compile* when
`Local::new`'s binding is put back).

Gates: `cargo test -p v8 --features simdutf` (182 in that binary, 6 new),
`cargo test --locked --workspace -- --skip
the_data_a_built_function_carries_survives_a_collection` — **5,116 passed / 0
failed / 4 ignored across 38 binaries** — and `cargo clippy --locked --workspace
--all-targets -- -D warnings` clean. No sweep, checked the same way as the slice
above: the engine change is two additive items (`api::Module` methods and the
graph walk), and no runner names `module_graph_has_tla`, `is_graph_async`,
`source_offset_to_location`, `deferred_namespace` or `request_count` (`grep` over
`test262`, `wasmtest`, `wasm`, `cli`).

Gates: `cargo test -p v8 --features simdutf` **164 passed / 0 failed**,
`clippy --locked --workspace --all-targets -- -D warnings` clean, and
`cargo test --locked --workspace` **5,098 passed / 0 failed / 4 ignored** across 38
binaries. `crates/v8` only, so no sweep is implicated.

**The static half of the template cluster — landed, 52 → 49, and the engine change it
needed is the interesting part.** Three sites: `FunctionTemplate::set` (a static
method, `bindings.rs:511`), `inherit` (`bindings.rs:537`) and
`ObjectTemplate::set_internal_field_count` (`jsruntime.rs:2854`).

The first two are one change, because the second is what makes the first need a
memo. This engine materialized a *fresh* function and a fresh `.prototype` object
on every `get_function`; the crate materializes one per context and hands the same
one back, and the difference shows exactly at `Inherit` — a child whose
`prototype.__proto__` is set to a *second copy* of the parent's prototype looks
identical to the right answer and fails `instanceof`. So a template now remembers
what each realm got (`Materialized { realm, function, pins }`), keyed by the
realm's handle address, and:

- the entry pins the realm as well as the function, because nothing in that `Vec`
  is traced and a swept realm's address could otherwise be reused by another realm
  — two realms answering as one is the hazard §5 records for an unrooted handle;
- the `.prototype` object needs no pin of its own: the function reaches it through
  its `prototype` property, and a pin traces what it reaches;
- `get_function` is idempotent per realm now, which is the crate's contract and
  what the test's `Child === ChildAgain` asserts.

`FunctionTemplate::set` landed as *data* properties only (the crate's static
accessor stays absent until a host asks; the accessor shape this engine has is the
one on an object template), applied at materialization, because that is when the
function object exists.

`set_internal_field_count` is the third, and it is a carried hint with the tier
said out loud where a host would look: the engine's objects have no internal-field
slots, the count is recorded and reported back (`internal_field_count`), and the
crate's `SetAlignedPointerInInternalField` and its reader stay absent rather than
present-and-wrong — a host that needs a slot has `Context`'s embedder data, which
the isolate does keep.

Measured: **52 → 49**, `E0599` 43 → 40. One new test, and it pins all three
halves: a static is a property of the function and not of its instances, an
instance of the child reaches the parent's prototype methods, and two
`get_function` asks are one function. Made to fail by dropping the inheritance at
materialization (`Object.getPrototypeOf(Child.prototype) === Parent.prototype`
goes from 1 to 0).

Gates: `cargo test -p v8 --features simdutf` **160 passed / 0 failed** with the
crashing test skipped, `clippy --locked --workspace --all-targets -- -D warnings`
clean, and `cargo test --locked --workspace` **5,095 passed / 0 failed / 4 ignored**
across 38 binaries. `crates/runtime` changed, inside `runtime::api` again, and no
runner names it (`grep` over `test262`, `wasmtest`, `wasm` and `cli` finds no
`FunctionTemplate`, `ObjectTemplate` or `runtime::api`) — the same check and the
same conclusion as the two `api` changes before it.

**The attribute-carrying half of the template cluster — landed, 56 → 52, and the
first engine change since the borrowed block.** Four sites: `ObjectTemplate::set_with_attr`
(`error.rs:2119`), `new_instance` (`cppgc.rs:59`, `error.rs:2155`) and
`set_accessor_property` (`bindings.rs:897`). Three of them needed the engine to
carry something it did not, which is the change to
`crates/runtime/src/api/template.rs`:

- **A template property carries its attributes.** `TemplateProperty::{Data,
  Accessor}` gained them and `apply` maps them onto the descriptor (`writable =
  !read_only`, `enumerable = !dont_enum`, `configurable = !dont_delete`).
  `PropertyAttributes` is the API type — the crate's `PropertyAttribute` said in
  the engine's own vocabulary, since the crate spells two of the three
  negatively. `ObjectTemplate::{set_with_attributes, set_accessor_with_attributes}`
  are the new entry points and the existing `set`/`set_accessor` delegate with the
  defaults, so no call site changed.
- **An accessor property can be built from two function templates.**
  `ObjectTemplate::set_accessor_rc` takes the callbacks *shared* rather than
  boxed, which is the shape two templates arrive in — their callback is the
  template's own — so the bridge passes the `Rc` along. Unwrapping it, which the
  first cut of this did, cannot work: the template is still holding one.
- **`FunctionTemplate::callback`** exposes that callback, and is why the accessor
  path needs no re-wrapping at all.

The bridge half is three methods on `LocalHandle<ObjectTemplate>`, plus the
attribute translation. The crate's `SetAccessorProperty` takes an `Option` for each
half and asserts that one is present; that assertion is reproduced, and the engine
is handed whichever half exists in the getter slot.

Measured: **56 → 52**, `E0599` 47 → 43.

Two new tests, one of them made to fail: an instance's properties with attributes
(a `DONT_ENUM | READ_ONLY` property is out of `Object.keys` and `writable: false`
in its own descriptor, while a plain one keeps the crate's defaults — ignoring the
attributes in `apply` reports `Object.keys(instance).length` as 2 where the test
wants 1), and an accessor property whose read runs the getter template's callback.

Gates: `cargo test -p v8 --features simdutf` **159 passed / 0 failed** with the
crashing test skipped, `clippy --locked --workspace --all-targets -- -D warnings`
clean, and `cargo test --locked --workspace` **5,094 passed / 0 failed / 4 ignored**
across 38 binaries. `crates/runtime` changed this time, so the sweep question was
asked rather than assumed: the change is inside `runtime::api`, no runner names
`runtime::api` at all (`grep` over `test262`, `wasmtest`, `wasm` and `cli`), and
only `runtime` and `crates/v8` name the methods — the same check, and the same
conclusion, §7 records for the microtask-policy change.

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
3. **The stragglers (3 → 0).** `PromiseRejectMessage` (1) **landed**, and it
   turned out to be a name the engine *can* back: the method it was hiding,
   `set_promise_reject_callback`, is now the one isolate callback that fires.
   `NearHeapLimitCallback` (1) **landed** as an accepted-and-not-fired shape.
   `SyntheticModuleEvaluationSteps` (1) **landed** with the synthetic-module
   surface below, once the engine had the record kind to hang it on.
4. **The rest of `v8::Module`** — method-level, so invisible in today's count.
   Landed since: `get_module_requests` (a `FixedArray` of `ModuleRequest`, which
   needed a payload-keyed `Data` downcast), `is_graph_async`,
   `source_offset_to_location` + `Location`, `get_module_namespace_with_phase`,
   and — with the surface below — `create_synthetic_module`,
   `set_synthetic_module_export` and the two `is_*_module` answers. Still open:
   `evaluate_for_import_defer` and `get_unbound_module_script` (no engine
   equivalent: the engine parses at load).

Every one of these is a real subsystem rather than a name to declare: the
inspector protocol, a second GC heap, a task runner, snapshots, structured
clone, stack traces, and cross-thread termination. Two did not fit that shape.
The **resolver** did not need an engine change: the engine resolves imports
itself from what the host has registered, so the bridge satisfies the callback
by walking the graph, asking the host, and registering the answers. The
**platform** turned out to be the other way round — a name whose honest
implementation is "held and never used", because there is no background work to
schedule; that is recorded in §9, not hidden in the code.

**The synthetic-module surface — landed, 25 → 21, one name and three methods.**
Four sites: `Module::create_synthetic_module` (`libs/core/modules/map.rs:650`,
`:716`), `Module::set_synthetic_module_export` (`:1794`) and the
`SyntheticModuleEvaluationSteps` type (`libs/core/runtime/bindings.rs:75`, cast
to a `c_void` for the snapshot's external references).

The engine gained the *record kind* this needed (§12 item 9, and the one engine
change this slice names): a `SyntheticModule` aspect on `SourceTextModule` (the
host's name for the record, the export names it declared with their cells, the
callback evaluation runs and the realm that callback is handed), the four
`api::Module` methods, and four dispatch points. Instantiation does nothing but
the status, which is V8's own `PrepareInstantiate`/`FinishInstantiate`
(`v8/src/objects/synthetic-module.cc:86`); evaluation links if it must, runs the
steps and records the promise they answer, keeping it so a second evaluation
answers the same promise rather than running them again, and records a failure
the way `RecordError` does (`:132`); namespace creation reads the declared names
instead of resolving them, because a synthetic record holds no bindings to
resolve and `resolve_export` would throw (`module.cc:358`); and a namespace read
answers the export cell, an unset export reading as *undefined* — which is what
the cell was created as. The kind was deliberately absent until now: a
JSON, text or bytes module is *wrapped* as `export default …` source
(`crates/runtime/src/module.rs:288-320`), and a host callback cannot be wrapped.

The bridge half is a shape question rather than a mapping, and it is where the
engine's `fn`-pointer callback earns its place. A record keeps its steps for its
whole life, so the engine stores a plain function pointer that names no lifetime
— and a host's steps cannot be closed over the way `instantiate_module`'s
callback is, because they outlive every scope the host has open when it declares
the module. So the bridge reaches the host's function through its *type*: `F:
UnitType + for<'s> Fn(..)` and an adapter generic over `F`, which is exactly what
`FunctionCallback` does for the same reason (a mapped function pointer is a value
whose type names the scope's lifetime, and that cannot be stored).
`MapFnFrom<F> for SyntheticModuleEvaluationSteps<'s>` keeps the crate's own
bound, because `bindings.rs` needs `map_fn_to()` and nothing else.

Two divergences, stated rather than implied. The crate's steps answer the empty
handle to report a throw; here that reads the isolate's pending exception and
becomes the engine's error, so a failing evaluation answers the rejection the
engine made from the failure where the crate answers the empty handle. And
`Module::create_synthetic_module` answers the handle directly, as there, but
aborts on an engine refusal: the shape has no failure channel, and the engine's
only way to fail there is refusing the empty program the record is built from —
which is the same result the crate's own `.unwrap()` panics on.

Measured: **25 → 21**, `E0599` 18 → 15 — one name and three methods, and the
remaining count is now 3 `E0425`s, 15 `E0599`s, 1 `E0282` and 2 `E0515`s. Two
new bridge tests, each made to fail: the happy path fails when the declared
export names are dropped (the steps' own `set_synthetic_module_export` then
answers `None`, `left: None, right: Some(true)`), and the failure path fails when
the adapter stops reading the pending exception (`left: Evaluated, right:
Errored`). The engine's two tests were verified the same way when they landed.

Gates: `cargo test -p v8 --features simdutf` **183 passed / 0 failed** (1
filtered: the known crashing test), `cargo test -p runtime --lib api::module` 5
passed, `cargo check -p v8 -p test262` (the feature-unification probe) clean,
`cargo clippy --locked --workspace --all-targets -- -D warnings` clean, and
`cargo test --locked --workspace -- --skip
the_data_a_built_function_carries_survives_a_collection` **5,120 passed / 0
failed / 4 ignored across 38 binaries**. One clippy finding this pass was in the
engine half itself (`synthetic.exports.borrow()[index].clone()` on a `Copy`
value) and is fixed rather than allowed.

An engine crate changed, so the battery was re-run rather than argued, and the
grep check is *not* the argument by itself this time: two of the dispatch points
(`create_namespace`, `namespace_get`) sit in paths every namespace read takes.
It holds anyway — no runner names `SyntheticModule`, `synthetic_module_create`,
`set_synthetic_module_export`, `synthetic_result` or `rejected_promise`, and
none reaches `runtime::api` — and the numbers are the certified ones: test262
`all` 48,622 fixtures, 48,464 pass, **0 fail / 0 crash / 0 hang**, 158 skip;
`intl402` 3,357 fixtures, 3,205 pass, 0 fail / 0 crash / 0 hang, 152 skip; the
eight wasm core suites **64,594 checks / 0 fail / 0 pending** (20,662 + 25,990 +
77 + 7,485 + 105 + 654 + 8,709 + 912); the JS-API sweep **1,001 tests / 0 fail**.

**What this exposes, and what the next record fixes.** The synthetic path is
name-complete, but `deno_core`'s own steps function did not type-check, and not
for a reason this slice owned: `map.rs:1805` was a second instance of §12 item 8
— a host helper returning a handle it made under a callback scope — previously
hidden behind the missing names. `E0515` went 1 → 2 for that reason, and the fix
is the one §12 item 8 described: the callback scope's first lifetime has to be
the parameter's rather than the storage borrow's. It is the record below.

**The callback scope's lifetime — landed, 21 → 19, both `E0515`s with it.** The two
sites (§12 item 8) were one shape: a host helper that opens a `callback_scope!`
and returns a handle it made inside, typed with its caller's lifetime.
`MapData::resolve_callback` (`libs/core/modules/map.rs:1374`) is the plain form;
`synthetic_module_evaluation_steps` (`:1805`) adds a `tc_scope!` and hands back a
resolver's promise — which is what exposed it, because the missing
synthetic-module names had been hiding the error behind them.

The crate we stand in for solves this in the *deref*, and says so in a comment
(`scratch/ref/v8-150/scope.rs:1968-1973`): a callback scope's handles "live as
long as the thing that we made the `CallbackScope` from", so
`PinnedRef<'_, CallbackScope<'i, C>>` derefs to `PinnedRef<'i, HandleScope<'i, C>>`
— the scope's own parameter, which `NewCallbackScope<'s> for Local<'s, Context>`
ties to the *context* — rather than to the borrow of its storage. This bridge's
deref kept the borrow, which is why a handle taken under a callback scope could
not leave it.

Two impls changed, and the cast between them is the interesting half: the
existing `cast_pinned_ref`/`_mut` helpers *preserve* the scope's lifetime, so all
they prove is the layout, while this one takes on a lifetime its caller claims —
`cast_pinned_ref_widening`, with the claim argued where it is made. The deref pair
widens to `'i`, and `NewTryCatch for PinnedRef<'obj, CallbackScope<'i, C>>` now
yields `TryCatch<'scope, 'i, HandleScope<'i, C>>` instead of threading the borrow
through: without that second half, a `tc_scope!` opened over a callback scope
would hand back handles typed with the borrow again, which is precisely deno's
steps function. `NewTryCatch`'s plain-`HandleScope` and `ContextScope` impls are
left alone — they thread the parent's own parameter, as there.

The consequence is stated rather than hidden, in `CallbackScope`'s documentation
and in `crate::store`'s: a host may now hold a handle past the scope it was made
in, which is what that shape means. The one payload this bridge ties to a region
is a *script's* source text, so a script handle that outlives its scope names a
released slot and panics ("a Script handle outlived its handle scope") rather
than reading a later script's text. The crate we stand in for carries the same
obligation with no check at all, because there the handle is a pointer into a
scope that is gone.

Measured: **21 → 19**, `E0515` 2 → 0, nothing new; the remaining 19 are 15
`E0599`s, 3 `E0425`s and 1 `E0282`. One new test, and it *is* the guard: it
settles a promise under a `tc_scope!` inside a `callback_scope!` and returns it
at the caller's own lifetime with no re-wrapping, so putting either impl back to
the borrow makes it fail to compile with the same `E0515` `deno_core` reported
("cannot return value referencing local variable `scope`" — checked by doing it).

Gates: `cargo test -p v8 --features simdutf` **184 passed / 0 failed** (1
filtered: the known crashing test), `cargo clippy --locked --workspace
--all-targets -- -D warnings` clean, `cargo test --locked --workspace -- --skip
the_data_a_built_function_carries_survives_a_collection` **5,121 passed / 0
failed / 4 ignored across 38 binaries**. No sweep, and the check is stronger than
a grep this time: no runner crate depends on `crates/v8` at all — no manifest
under `crates/{test262,wasmtest,wasm,cli}` names it and `cargo tree -p test262`
shows none — so the battery cannot see this change either way.

**The compiled-module surface — landed, 19 → 15, and the first wasm the bridge
has ever compiled.** Four sites: `v8::WasmModuleObject::compile`
(`libs/core/modules/map/wasm.rs:48`), `module.get_compiled_module()`
(`ops_builtin_v8.rs:704`), `WasmModuleObject::from_compiled_module` (`:799`) and
the `CompiledWasmModule` name (`runtime/jsruntime.rs:507`, in the cross-isolate
store's type).

The engine already compiled wasm: a `WebAssembly.Module` object is an ordinary
object whose decoded `wasm::Module` sits in `agent.wasm_modules` under the object's
id (`crates/runtime/src/builtins/wasm.rs:808-836`). So the engine change is an
*exposure* — `api::WasmModuleObject::{compile, from_compiled_module,
get_compiled_module}` over a new `api::CompiledWasmModule`, plus one `pub(crate)`
split in `builtins/wasm.rs` so that the JS API's own `WebAssembly.compile` and a
host's `compile` build their object the same way (prototype and [[Module]] slot
alike). The refactor is behaviour-preserving by construction: `compile_module_bytes`
now ends in the same `module_object_with` the new entry points call, and no
existing caller changed path. `crates/v8/Cargo.toml` also changed, and that is the
mechanism that makes any of it reachable: it now asks `slag` for its `wasm`
feature, because the crate we stand in for always has WebAssembly — a host never
asks for it and must not have to.

`api::CompiledWasmModule` is the decoded module, not a handle to a shared
allocation, and that is what makes it work for what a host does with it: it points
at nothing in the isolate's arena, so it outlives the object it came from and can
travel to another isolate — the two properties `deno_core`'s `CrossIsolateStore`
needs, and the reason upstream marks its own `Send + Sync`. What it does not carry
is the wire bytes, so `get_wire_bytes_ref` is absent rather than wrong.

Measured: **19 → 15** (`E0599` 15 → 12, `E0425` 3 → 2). Two bridge tests, each
verified by mutating the code it guards: the round trip fails when
`from_compiled_module` answers a default module (`left: Module { … exports: [] }`
against the real one, memory and `m` export), and the failure path fails when
`compile` stops recording the exception (`assertion failed: tc_scope.has_caught()`).
The round trip also pins what a host's `instanceof WebAssembly.Module` will say,
by comparing the object's prototype with `WebAssembly.Module.prototype` — dropping
the module prototype in `compile_module_value` fails it (`left:
Payload::Value(Local(Null))`). The bytes the tests compile are hand-built — 20 of
them: header, a one-page memory, `m` exported as memory 0 — which is what makes
the round trip's equality meaningful.

Gates: `cargo test -p v8 --features simdutf` **186 passed / 0 failed** (1 filtered:
the known crashing test), `cargo clippy --locked --workspace --all-targets -- -D
warnings` clean, `cargo test --locked --workspace -- --skip
the_data_a_built_function_carries_survives_a_collection` **5,123 passed / 0 failed
/ 4 ignored across 38 binaries** (the root `Cargo.lock` is untouched by any of
this). `crates/v8` *and* `crates/runtime` changed, and the engine half sits in
exactly the path the wasm JS-API sweep exercises, so the battery ran rather than
being argued: test262 `all` 48,622 fixtures — 48,464 pass, **0 fail / 0 crash / 0
hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 0 fail / 0 crash / 0 hang, 152
skip; the eight wasm core suites **64,594 checks / 0 fail / 0 pending** (20,662 +
25,990 + 77 + 7,485 + 105 + 654 + 8,709 + 912); the JS-API sweep **1,001 tests / 0
fail**. Every number is the certified one.

One trap this slice hit twice, recorded because it costs time: the wasm core suite
is invoked as `wasmtest run --strict waspec/test/core/*.wast`. A doubled slash —
`core//*.wast`, which is what an unset shell variable produces inside a loop —
globs into the subdirectories as well and reports 20,736 pass with 2 "fails" that
are the same suites reached twice.

**The streaming half — landed, 15 → 12, and the last wasm name is gone.** Three
sites, all one subsystem: `WasmStreamingResource` (`libs/core/ops_builtin.rs:263`,
whose `RefCell<v8::WasmStreaming<false>>` is what `op_pipe` writes chunks into),
the isolate-wide callback (`ops_builtin_v8.rs:1414`), and its installation at
`runtime/setup.rs:302`. What V8 does is fixed (`v8/src/wasm/wasm-js.cc:889-911`):
`compileStreaming` makes a resolver, resolves the source on a later turn, and
hands the embedder the resolved value together with a `WasmStreaming` it feeds
and finishes; V8 `DCHECK_NOT_NULL`s there, because its `WebAssembly` namespace is
always installed.

Engine side this is a host-driven path rather than a new decoder, and it is the
first hook that had to *ask permission*: `HostHooks` gained
`has_wasm_streaming_callback` and `wasm_streaming`
(`crates/runtime/src/host.rs`), `WebAssembly.compileStreaming` routes through
them, and an isolate whose host installed neither refuses the call with a
`TypeError` instead of answering a promise nothing could settle. The two handlers
that chain builds — the resolve and the reject side V8's
`StartAsyncCompilationWithResolver` attaches the same way — are reached by their
own function identity through a table on the agent, the shape `async_await`'s
resume handlers already use. The record is
`api::WasmStreaming` (`crates/runtime/src/api/wasm.rs`): accumulated bytes, the
url, the promise capability and a settled flag, with the three promise values
**pinned** for as long as a host holds the stream — nothing in the engine points
at them, since the two handlers that do are single-use and die with the source
promise's reactions. `finish` decodes then and settles the promise; `abort`
rejects it, or leaves it unsettled when given no value — V8's own behaviour, and
the one that matters for a browser tab being refreshed (`wasm-js.cc:87`).

Bridge side, the surface is `crate::wasm::WasmStreaming<const bool>` (upstream's
const parameter is kept because a host's own code names the type with it; what
`true` selects — `set_has_compiled_module_bytes` and the caching callback `finish`
takes — is absent here rather than wrong, because this engine has no
compiled-module cache to hand bytes back to), and
`Isolate::set_wasm_streaming_callback<F>`, whose bounds are upstream's exactly
(`F: UnitType + for<'a,'b,'c> Fn(&'c mut PinScope<'a,'b>, Local<'a, Value>,
WasmStreaming<false>)`). The plan expected an `IsolateWasmStreamingCallback` alias
to mirror; **there is none upstream** — v150.4.0 takes the host's function as a
bare generic — so the bridge has none either, and the first `deno_core` check with
this in place resolved all three sites at once.

Three findings worth more than the code:

- **The installed callback's parameters must share one lifetime**, and that is not
  a style choice. The type being satisfied is higher-ranked
  (`for<'a,'b,'c> Fn(&'c mut PinScope<'a,'b>, Local<'a, Value>, …)`), so a fn item
  whose scope and source lifetimes are independently elided is *universally*
  quantified over both and cannot be proven to satisfy it (`argument requires that
  '1 must outlive '2`, with `&mut` invariance over the scope's type parameter
  making the two unprovable rather than merely awkward). Deno's own callback
  spells it the way the bound requires — `wasm_streaming_callback<'a>(scope: &mut
  v8::PinScope<'a, '_>, arg: v8::Local<'a, v8::Value>, …)` — and the bridge's
  trampoline shares that parameter, which is what lets the fn item coerce to the
  stored fn pointer.
- **Two isolate callbacks, one hook implementation, so neither may displace the
  other.** The seam is a single `HostHooks` implementation, and the
  promise-rejection callback used to live *in* the bridge's hooks struct — which
  would have meant that a host installing a streaming callback dropped it. The
  struct is stateless now and each callback is stored on the isolate that
  installed it, read back at call time; `install_hooks` is the one place that
  claims the seam.
- **`finish`/`abort` abort the caller on an engine refusal.** There is no channel
  to report through — the crate we stand in for's versions answer `()` — and the
  one way either can fail, a stream whose realm is already gone, is a bridge bug
  rather than a host's mistake. Stated in both methods rather than left to be
  discovered.

Measured: **15 → 12** (`E0599` 12 → 11, `E0425` 2 → 0; the `E0282` is untouched
and is not this subsystem's). Two bridge tests, each verified by mutating the code
it guards. `a_host_streaming_callback_settles_compile_streaming` streams the same
hand-built 20-byte module the compiled-module slice uses and asserts the promise
is `Pending` before the microtask drain and `Fulfilled` after, with
`exports[0].name == "m"`; making `BridgeHooks::wasm_streaming` return before
calling the host's callback fails it (`left: Pending, right: Fulfilled`).
`installing_one_isolate_callback_keeps_the_other` installs both callbacks, in the
order a host makes them, and exercises both halves afterwards; making
`install_hooks` clear `promise_reject` fails it ("the rejection callback installed
first still runs"), which is the second finding above as a test.

Gates: `cargo test -p v8 --features simdutf` **189 passed / 0 failed** (2 of them
this step; the known crasher passed on this run rather than being filtered),
`cargo clippy --locked --workspace --all-targets -- -D warnings` clean (one
`clone_on_copy` the new test code had to answer for), `cargo test --locked
--workspace -- --skip the_data_a_built_function_carries_survives_a_collection`
**5,128 passed / 0 failed / 4 ignored across 38 binaries**. `crates/runtime`
changed again, and in the `WebAssembly` namespace the JS-API sweep exercises, so
the battery ran rather than being argued: test262 `all` 48,622 fixtures — 48,464
pass, **0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 0
fail / 0 crash / 0 hang, 152 skip; the eight wasm core suites **64,594 checks / 0
fail / 0 pending**; the JS-API sweep **1,001 tests / 0 fail**. Every number is the
certified one.

The wasm tail is closed. What was left of the bridge's frontier then was the
method
surface: 5 `E0599`s (`Module::evaluate_for_import_defer`,
`Module::get_stalled_top_level_await_message`, `StackTrace::current_stack_trace`
twice, and `HandleScope::get_heap_statistics`), and the one `E0282` nothing had
explained.

**The unbound scripts and the code cache — landed, 12 → 6, and the one that made
`None` unavailable.** Six
sites, three names: `Script::get_unbound_script` (`libs/core/runtime/jsrealm.rs:575`
and `ops_builtin_v8.rs:398`), `Module::get_unbound_module_script`
(`libs/core/modules/map.rs:904` and `:918`), and `create_code_cache` twice — on
the unbound script (`ops_builtin_v8.rs:550`) and on a function
(`libs/core/modules/map/ext_script.rs:711`).

The setup is what made this the next slice rather than any of the other
remaining ones, and it is worth reading before the code: **V8's code cache is
load-bearing in `deno_core`'s default configuration.** The module map asks for a
cache on *every* module load — a cache *miss* included
(`modules/map.rs:900`, guarded by `try_store_code_cache`, which the no-data
branch sets `true`) — and turns `None` into
`ModuleError::Concrete(UnboundModuleScriptCodeCache)`, i.e. a failed load, and
the CLI enables caching unless `--no-code-cache` is passed
(`cli/args/flags.rs:680`). So the crate's own documented answer for a script that
cannot be serialized (`None`, "This will return nullptr if the script cannot be
serialized") is not one this bridge can give, however truthful it would be on its
own.

What it gives instead is the code's own **source text**. The engine keeps no
serialized compiled form — it parses and evaluates in one pass — so there is
nothing else it *could* give; V8's contract makes the bytes opaque to a host; and
`CachedData::rejected()` answers `true` always, which is the flag's own escape
hatch: a host that trusts it re-produces its cache, and nothing is ever believed
that was not checked. The cost is stated where a host would look for it (the
module header of `crates/v8/unbound_script.rs`): its cache database holds a copy
of the source and the round trip saves nothing. A cache that saves something
means serializing the compiled program, which §10 already lists as engine-side
build-order work — not something a bridge can fake into being faster.

`CachedData` gained an owned form for that (`crates/v8/script_compiler.rs`),
because `create_code_cache` answers `CachedData<'static>` — data that outlives
the call that made it, where `new` wraps bytes a host lends for a compile.

**The unbound forms are the identity, and that is the engine's shape rather than
a shortcut.** There, a `Script` is bytecode bound to a context and an
`UnboundScript` is the same bytecode unbound; here a script *is* its source text
(`crates/v8/script.rs`) and a module *is* a record, and neither names a context —
every script handle is already unbound, so `bind_to_current_context` returns the
handle it was given and `get_unbound_script` / `get_unbound_module_script` retag
it. No engine state was needed for any of that, and a synthetic module — which
has no script, and which the crate `DCHECK`s against — aborts with that reason.

`get_source_mapping_url` came with it, because the same name the fix unmasks is
what `modules/map.rs:919` reads to find a module's source map. V8 reads a *magic
comment* as syntax (`Scanner::TryToParseMagicComment`,
`v8/src/parsing/scanner.cc:280`: `//[#@]\s*sourceMappingURL\s*=\s*<url>`, last one
wins) and the bridge has no lexer to do that with — the engine's parser keeps its
comments to itself, and its only public surface is the parse functions — so it
reads the source as text with one restriction that keeps a false positive very
unlikely: the comment must be on a line of its own, which is how every emitter
writes one. §9 states that as a divergence, both directions.

**Engine change, as named before it was written (§9's ledger, item 11): two
exposures, no new state.** `api::Isolate::function_source` reads
`agent.ecma_functions`' stored definition text — the same text
`Function.prototype.toString` answers with, which is also why a builtin, whose
body is a builtin, answers `None` — and `api::Module::source_text` reads
`SourceTextModule.source`, kept for the same reason. Neither changes any
behaviour; the battery below ran anyway, because the engine crate changed.

Measured: **12 → 6** (`E0599` 11 → 5; the `E0282` is untouched and is not this
subsystem's). Six bridge tests, each verified by mutating the code it guards:
an empty cache instead of the source text fails the script test on the bytes; a
fixed payload fails the module test; answering the *first* magic comment instead
of the last fails the ordering assertion; making `source_text` answer for a
synthetic module fails the synthetic-module abort; making the source map answer
of a comment-free source a string instead of `undefined` fails two tests; and a
`function_source` that always answers `Some(String::new())` fails both halves of
the function test (the definition text and the builtin's `None`).

Gates: `cargo test -p v8 --features simdutf` **195 passed / 0 failed** (6 of them
this step), `cargo test -p runtime --lib` **787 passed / 0 failed**,
`cargo clippy --locked --workspace --all-targets -- -D warnings` clean (one
`trim_split_whitespace` the scan had to answer for), `cargo test --locked
--workspace -- --skip the_data_a_built_function_carries_survives_a_collection`
**5,134 passed / 0 failed / 4 ignored across 38 binaries**. `crates/runtime`
changed, so the battery ran and every number is the certified one: test262 `all`
48,622 fixtures — 48,464 pass, **0 fail / 0 crash / 0 hang**, 158 skip;
`intl402` 3,357 — 3,205 pass, 0 fail / 0 crash / 0 hang, 152 skip; the eight wasm
core suites **64,594 checks / 0 fail / 0 pending**; the JS-API sweep **1,001 tests
/ 0 fail**.

What is left is 3 errors and no straggler: the stalled `await` report,
import-defer's evaluation entry point, and `get_heap_statistics`. The `E0282` that
had stood unexplained since the serializer step was the untested
`escapable_handle_scope!` macro, and the step below closes it.

**The stack-trace frames — landed, 6 → 4, and the step that measured the plan
wrong.** Two sites (`ops_builtin_v8.rs:1518`, `modules/import_graph.rs:164`), and
§10 had called this the next engine item with a survey that split it in two: the
*execution-context frame accessor* and the *per-activation source positions*.

**The engine has no call stack, and `execution_context_stack` is not one.**
The plan assumed that stack *is* the activations a trace would report. It is the
spec's execution-context stack, and the engine does **not** push a context per
call: an ordinary call runs its body on the VM's own environment stack, and a
context is pushed where the spec needs one — a script, a module, eval, an async
or generator resumption — and, on the call paths that need it, for a body that
reads a spec-only component. Measured with a probe over five shapes of the same
call, the deciding case being a sloppy `arguments` (which is exactly where the
engine says it pushes one, `crates/runtime/src/function.rs:2271-2278`):

| source | frames reported |
|---|---|
| `function inner() { return capture(); } inner();` | `["-@-"]` |
| the same, called indirectly as `(0, inner)()` | `["-@-"]` |
| the same, with `inner(a) { if (a) return arguments; … }` | `["inner@-", "-@-"]` |
| `const inner = () => capture(); inner();` | `["-@-"]` |
| `function a() { return b(); } function b() { return capture(); } a();` | `["-@-"]` |

The probe is now the bridge's test (`a_capture_reports_the_engines_contexts_not_the_call_stack`),
which asserts those two shapes so the difference cannot rot silently. What it
means for a host, and why the surface still shipped: a trace here answers "which
code am I in" exactly — the script or module a frame belongs to is what it names
— and answers "how did I get here" only as far as the engine keeps activations.
Closing that gap means the VM's own interpreter and JIT frames, which is a
performance-sensitive engine feature rather than a bridge change, and it is what
§10's "structured frames" item now means.

**What landed, engine side (named in §9's ledger as item 12 before it was
written):**

- `SourceTextModule` gains `name: Option<JsString>`, set at parse time —
  `parse_module` takes it, `host_resolve_imported_module` passes the specifier it
  resolved the source under, and a new `api::Module::compile_with_name` threads
  the host's own `ScriptOrigin` name, which the bridge's `compile_module2` had in
  hand and was discarding (it passed `""`). Three call sites in this workspace
  name it now (`crates/test262` passes `None`, and the test262 fixture key is not
  a name a frame should report). A *classic script* still has no name: naming one
  means threading a name through the eval path (`Context::eval` →
  `Agent::run_script` → `parse_script`), which every runner uses, and no site in
  the frontier asks for it. Recorded as an open item rather than half-threaded.
- `Agent::stack_traces` holds each capture's frames under the **box address of
  the object the capture mints**, and `Agent::compact_weak_tables` prunes a dead
  key — the one place in this workspace where an identity-keyed table is
  *bounded* rather than growing forever (§12 item 7 is that same problem for the
  bridge's position table, still open). The hook runs between the mark and the
  sweep, so a dead address cannot yet have been reused, and the dead set is the
  *precise* one, so a stale stack word cannot keep a capture alive: the test that
  checks the pruning depends on both facts.
- `api::Isolate::{capture_stack, captured_frame_count, captured_frame}` over a
  new `api::StackFrame`.

**What is deliberately absent, as the tier rather than as a gap:** line and
column are 0 (V8's `Message::kNoLineNumberInfo`/`kNoColumnInfo`) because
positions are the survey's *other* half and nothing records them per activation;
`is_eval`, `is_constructor` and `is_wasm` are `false` because an execution
context does not record them (eval code even inherits its caller's
script-or-module, so it cannot be told apart by that either); and
`is_user_javascript` is `true` because the engine's Rust builtins never push a
context and it classifies no script as native. §9 states each.

Bridge side: `crates/v8/stack_trace.rs` with `StackTrace::current_stack_trace`
(the crate's static, so it is an inherent `impl` on the tag) and the frame
accessors `deno_core` names — `get_frame_count`, `get_frame`,
`is_user_javascript`, `get_line_number`, `get_column`, `get_script_name`,
`is_eval`, plus `get_function_name`, `get_script_id`,
`get_script_name_or_source_url`, `is_constructor` and `is_wasm` because leaving
half a frame's accessors out would be a worse shape than answering them — and a
`Payload::StackFrame { trace, index }` variant, the shape `ModuleRequest` already
had. A frame handle is a *position in a capture*, so reading one whose capture
the collector reaped answers the no-information value rather than a stale frame.

Measured: **6 → 4** (`E0599` 5 → 3; the `E0282` untouched here), and nothing was
unmasked behind it — every accessor `deno_core` names on a frame exists. Six new
tests, each verified by mutating the code it guards: including the bootstrap
context as a frame fails all three bridge tests; answering `None` for every
script name fails the module test; refusing to prune fails the collection test;
and removing the zero-limit refusal fails that one.

Gates: `cargo test -p v8 --features simdutf` **198 passed / 0 failed** (3 of them
this step), `cargo test -p runtime --lib` **790 passed / 0 failed**,
`cargo clippy --locked --workspace --all-targets -- -D warnings` clean,
`cargo test --locked --workspace -- --skip
the_data_a_built_function_carries_survives_a_collection` **5,140 passed / 0
failed / 4 ignored across 38 binaries**. `crates/runtime` *and*
`crates/test262` changed (the `parse_module` signature reaches the runner), so the
whole battery ran and every number is the certified one: test262 `all` 48,622 —
48,464 pass, **0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205
pass, 0 fail / 0 crash / 0 hang, 152 skip; the eight wasm core suites **64,594
checks / 0 fail / 0 pending**; the JS-API sweep **1,001 tests / 0 fail**.

**The escapable handle scope — landed, 4 → 3, and the `E0282` explained at last.**
The one error that had survived every step since the serializer ("nothing has
explained it yet", recorded three times) was `libs/core/runtime/jsruntime.rs:1676`:

```rust
v8::escapable_handle_scope!(let scope, scope);
```

`cannot infer type`, at the macro. **The macro had never been exercised anywhere in
this workspace** — it is defined in `crates/v8/scope.rs` and used by no test and
no bridge code — and it does not type-check: `EscapableHandleScope::new` is a
constructor whose three type parameters (`'s`, `'esc`, `C`) are named by the
*type* and by nothing in the argument, so at the call `'esc` (and `C`, for the
isolate form) had no constraint at all to be inferred from. `deno_core`'s
`JsRuntime::eval` is the one user, which is why the error looked like it came from
somewhere else for eight steps.

The fix ties them to the argument: `new` now requires
`P: NewEscapableHandleScope<'s, NewScope = EscapableHandleScope<'s, 'esc, C>>`, so
the associated type every constructor already implements determines both. The
guard is a bridge test in the shape `deno_core` writes it — an escapable scope
over the scope a function was handed, with a script's value escaping to the
caller's lifetime — and it is a *compile-time* guard: reverting the bound stops
the crate compiling (verified by doing it). Nothing about it is a divergence from
the crate we stand in for; it is a defect this bridge had and no test reached,
which is the same lesson as the untested `Array::new` length in §12 item 5.

Gates for this step: `cargo test -p v8 --features simdutf` **199 passed / 0
failed**, `cargo clippy --locked --workspace --all-targets -- -D warnings` clean,
`cargo test --locked --workspace -- --skip
the_data_a_built_function_carries_survives_a_collection` **5,141 passed / 0 failed
/ 4 ignored across 38 binaries**. **No engine crate changed** — `crates/v8` only,
which no runner depends on — so the sweep battery is not implicated, and the
numbers from the step above stand.

**The heap statistics — landed, 3 → 2, and every number is one the engine already
kept.** One site: `op_memory_usage` (`libs/core/ops_builtin_v8.rs:1362`), which is
`Deno.core.memoryUsage()`, reading four of the crate's fifteen `HeapStatistics`
accessors.

The question a host's four numbers raise is what a "heap" is here, and the
answer is not V8's: a chunked bump arena that grows on demand, never reserves
beyond what it commits, and has no limit. So `total_heap_size` and
`total_physical_size` are the *same* number — the committed bytes — and that is
stated rather than faked into two. `used_heap_size` is the bytes the live boxes
occupy, counted by the same arena walk the collector uses (`for_each_live`), each
box's own footprint without the bytes of anything it points at, and swept slots
excluded. `external_memory` is the interesting one: an `ArrayBuffer`'s storage is
an `Rc<Vec<u8>>` in `crates/byteblock`, outside the arena and outside any
counter, so the arena's numbers cannot see it — but the engine keeps its own
record of every live buffer object (`Agent::buffer_data`), and summing that is
exact rather than guessed: a detached buffer has no bytes and a *borrowed* block
(the host pointer path) is the host's memory, not this engine's.

**What the crate's other eleven accessors are: absent.** `heap_size_limit`,
`total_available_size` (no limit), `malloced_memory`, `peak_malloced_memory`,
`total_allocated_bytes` (no allocation total is kept),
`total_global_handles_size`, `used_global_handles_size` (no handle registry),
`total_heap_size_executable` (no code lives in the heap — the JIT's code is
Cranelift's own allocation) and `does_zap_garbage` have no honest value, so a host
that names one gets a compile error. That is the same rule every earlier slice
followed, and §9 records it as the tier.

Engine change, as named before it was written (§9's ledger, item 13): two
read-only accessors on `crux::heap::Heap` (`committed_bytes`, `live_bytes` — the
latter the existing arena walk, so no allocation path gained a write), a new
`api::HeapStatistics`, and `api::Isolate::heap_statistics`. Bridge side: a
`crates/v8/heap.rs` with the four accessors as methods and
`Isolate::get_heap_statistics`, which is reachable from a *scope* through the
deref chain — the receiver `deno_core` uses, and the test uses it that way.

Measured: **3 → 2** (`E0599` 3 → 2), with nothing unmasked. Three new tests, each
verified by mutating the code it guards: a `committed_bytes` of 0 fails the arena
test, a doubled byte length fails the external-memory test (which is also the test
that showed a first version of that test could not fail — a *view* was asserted
about as if it were a buffer object, and the dedupe it was supposed to exercise
turned out to be unreachable, so the dedupe is gone and the test now says what it
checks), and a `total_physical_size` of 0 fails the bridge test.

Gates: `cargo test -p v8 --features simdutf` **200 passed / 0 failed**,
`cargo test -p runtime --lib` **792 passed / 0 failed**, `cargo test -p crux --lib`
**248 passed / 0 failed**, `cargo clippy --locked --workspace --all-targets -- -D
warnings` clean, `cargo test --locked --workspace -- --skip
the_data_a_built_function_carries_survives_a_collection` **5,144 passed / 0 failed
/ 4 ignored across 38 binaries**. `crates/crux` and `crates/runtime` changed, so the
battery ran and every number is the certified one: test262 `all` 48,622 — 48,464
pass, **0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 0 fail
/ 0 crash / 0 hang, 152 skip; the eight wasm core suites **64,594 checks / 0 fail /
0 pending**; the JS-API sweep **1,001 tests / 0 fail**.

**The module-graph tail — landed, 2 → 0, and the crate type-checks.** Two
sites, both reaching machinery the engine already had.

`Module::evaluate_for_import_defer` (`libs/core/modules/map/dynamic.rs:604`, the
`import.defer()` path). V8's own body (`v8/src/api/api.cc:2473`) gathers the
module's asynchronous transitive dependencies; with none it resolves a *fresh*
promise with the deferred namespace and returns it — already fulfilled — and
otherwise evaluates each dependency and performs `Promise.all` over their
evaluation promises. The engine's deferred-import protocol does all of it
already (`gather_async_transitive_dependencies`, the countdown waiters, the
deferred namespace), so what was missing was the entry point rather than a
mechanism: the `.then` job's tail was extracted into `await_async_dependencies`
so the same wait is reachable without the DeferredModule object, and the entry
point resolves the empty case *before returning*. That last part is the one
`deno_core` reads — its caller branches on `promise.state()` to tell "nothing to
wait for" from "still waiting" (`dynamic.rs:621`) — and resolving through the job
instead would answer `Pending` where V8 answers `Fulfilled`, which is exactly
what the first mutation of this slice showed.

`Module::get_stalled_top_level_await_message`
(`libs/core/modules/map/evaluation.rs:347`). V8 walks the graph
(`SourceTextModule::InnerGetStalledTopLevelAwaitModule`,
`v8/src/objects/source-text-module.cc:1630`) and reports a module with no pending
async dependency and an async-evaluation ordinal — one suspended on its own
top-level await. The engine's `ModuleStatus::EvaluatingAsync` with
`pending_async == 0` is that same state (the body has begun and there is nothing
left to wait for), so the walk is the shape `module_graph_has_tla` already had:
the root first, then the *evaluation*-phase requests that were linked, stopping
at a module it reports. The message has no exception behind it, so the bridge
mints one — `Payload::TemplateMessage { module }`, whose text is the crate's
`kTopLevelAwaitStalled` template and whose identity is the module it reports on.

Five new tests, each verified by mutating the code it guards: taking the
non-empty branch unconditionally fails both deferred-import tests (`left:
"pending", right: "fulfilled"`); a pending-dependency count of 1 fails both
stalled tests; a wrong template text fails the bridge text assertion; a negated
payload-equality arm fails `message == again`; and a defer-phase request filter
fails the graph-descent assertion.

Gates: `cargo test -p v8 --features simdutf` **203 passed / 0 failed**,
`cargo test -p runtime --lib` **794 passed / 0 failed**, `cargo test -p crux
--lib` **248 passed / 0 failed**, `cargo clippy --locked --workspace --all-targets
-- -D warnings` clean, `cargo test --locked --workspace -- --skip
the_data_a_built_function_carries_survives_a_collection` **5,149 passed / 0
failed / 4 ignored**. `crates/runtime` changed, so the battery ran and every
number is the certified one: test262 `all` 48,622 — 48,464 pass, **0 fail / 0
crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 0 fail / 0 crash / 0
hang, 152 skip; the eight wasm core suites **64,594 checks / 0 fail / 0
pending**; the JS-API sweep **1,001 tests / 0 fail**.

`cargo check -p deno_core` from `deno/` is **0 errors**. That is a *compile*
milestone and nothing more — no host has been *run*, and §9's divergences say
what one would find if it were. What the metric never measured is what stands
between the two: the runtime gaps this work found (`queueMicrotask`, a host that
must boot without a snapshot), which is §10's (3).

**Deno itself, measured — the frontier moved, and it is not `deno_core`.** With
the boundary clean, the next question was whether the *CLI* compiles, and it does
not. `cargo check -p deno` (the CLI's reachable graph) reports **113 errors**, and
`cargo check --workspace --keep-going` (which reaches the members the CLI's
graph never got to, because a failed dependency stops the walk) reports **341
across eight crates**: `deno_web` 83, `deno_telemetry` 213, `deno_node_sqlite`
19, `deno_webgpu` 14, `deno_crypto` 6, `deno_ffi` 3, `deno_os` 2,
`deno_inspector_server` 1. Those are cargo's own totals from its summary lines;
counting raw diagnostics instead doubles every one of them, because the workspace
builds each crate in two feature-unified units. `deno_telemetry`'s 213 are almost
all a single surface — `ValueView` and `ValueViewData`, 196 of the 213 — with a
dozen GC-callback methods (`add_gc_prologue_callback`,
`add_gc_epilogue_callback`, `get_heap_space_statistics`,
`get_number_of_data_slots`, `Isolate::from_raw_isolate_ptr_unchecked`) behind
them. **None of them is `deno_core`, and `deno_runtime` and
the CLI were never attempted** — they depend on the crates that failed — so the
total is a lower bound in exactly the way §7's name frontier was. Every failing
crate is the *host's own op library*, not the engine boundary: the frontier is
now "what the ext crates name", which is a larger surface than what `deno_core`
names.

The classes, which matter more than the count:

| class | surface | where |
|---|---|---|
| **engine capability** | `TracedReference`, `Weak` | `ext/web` (console, geometry, image data), `ext/node_sqlite` |
| **engine capability** | `ValueView`, `ValueViewData` | `ext/telemetry` (the bulk of its 213) |
| bridge-only | the simdutf **base64** half (`Base64Options`, `LastChunkHandling`, `base64_to_binary`, `base64_length_from_binary`, `maximal_binary_length_from_base64`) | `ext/web/lib.rs` |
| bridge-only | `GCType`, `GCCallbackFlags`, `IntegrityLevel` + `Array::set_integrity_level`, `TimeZoneDetection` + `Isolate::date_time_configuration_change_notification`, `VERSION_STRING` | `ext/web`, `ext/os`, `ext/inspector_server` |
| bridge-only | the method tail: `Symbol::description`, `Value::{type_of, instance_of}`, `Object::{get_own_property_descriptor, preview_entries}`, `Set::size`, `Map::size`, `TypedArray::length`, `Date::value_of`, `SharedArrayBuffer::byte_length`, `String::{new_external_onebyte, write_utf8_v2}` | `ext/web/console` |
| shape | `NewHandleScope for PinnedRef<CallbackScope>` (6), two `E0512` transmute-size mismatches, one `Rc<BackingStore>: Send` | `ext/ffi`, `ext/node_sqlite`, `ext/web/broadcast_channel.rs` |

The first row is the plan's own **L2 weak-persistent-handle item**, recorded in
§12 item 1 as "not started" — `deno_core` never names it, so the `deno_core`
metric could never have reached it, and the ext crates make it a *blocker for
Deno* rather than a tidy-up. That is the most useful thing this measurement
bought.

**And the engine is already being run.** Two build scripts — `dcore`'s and
`libs/core/examples/snapshot`'s — execute a JavaScript bootstrap to create a
startup snapshot, and they get as far as `JsRuntimeForSnapshot::try_new` before
panicking at `libs/core/runtime/bindings.rs:316` ("unable to convert"). The
backtrace puts it in `initialize_deno_core_namespace`, reading
`ExtrasBindingObject.console` with `bindings::get::<Local<Object>>`: the property
is absent, so the read answers `undefined` and the conversion to `Local<Object>`
fails. The engine's extras binding object exists (§12 item 9, where the work to
make it a per-context object landed) but nothing populates its `console`, which
in V8 is a C++ extras binding. So the first *runtime* blocker is a named thing
rather than a mystery: the extras binding object needs the `console` V8 puts
there, and after that the bootstrap has the rest of deno_core's JS to get
through.

**The extras binding console — landed, and the bootstrap runs past it.** The first runtime blocker the measurement above named: `initialize_deno_core_namespace` reads `ExtrasBindingObject.console` (`runtime/bindings.rs:373`) and the engine's extras binding object had nothing there. V8 fills it in `Genesis::InitializeConsole` (`v8/src/init/bootstrapper.cc:5617`), so the bridge now builds that object and puts it in both places V8 does — the extras binding object, and `globalThis` (`01_core.js:827` reads the latter), each with the `DONT_ENUM` attributes V8 gives it.

The methods are inert, and that is the faithful answer rather than a shortcut: V8's own console methods return before doing anything when the isolate has no console delegate (`v8/src/builtins/builtins-console.cc:158`), and the crate we stand in for exposes no way to install one — so `deno_core`, whose `callConsole` calls *both* the V8 method and its own (`runtime/bindings.rs:1753`), prints once. A console here that printed would double every message. Their shape is V8's too: zero-length, named after the property, enumerable (the bootstrap walks them with `Object.keys`, `01_core.js:756`) and with no `[[Construct]]`, so `new console.log()` throws the `TypeError` V8's throws.

Two tests (one replacing the old "the extras binding object starts empty", which documented the tier this change ends), each verified by mutating the code it guards: dropping the constructor behaviour fails with `left: "constructed"`, dropping the name fails with `left: ""`, and installing the console on the extras binding object alone fails the global-object assertion.

Gates: `cargo test -p v8 --features simdutf` **204 passed / 0 failed**, `cargo clippy --locked --workspace --all-targets -- -D warnings` clean, `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,150 passed / 0 failed / 4 ignored**. `crates/v8` changed and nothing in this workspace depends on it — checked rather than assumed, since no member names it — so the battery is not implicated and the previous step's certified numbers stand.

**And the bootstrap moved.** Re-measured on the same tree, `deno/`'s two snapshot build scripts now reach a *JavaScript* failure instead of a conversion one:

    SyntaxError: Module ext:core/ops does not export op_log_debug

`op_log_debug` is a host op (`libs/core_testing/checkin/runner/extensions.rs`) that the harness's `ext:core/ops` synthetic module is meant to export, and `checkin/runtime/console.ts` imports — so what fails is the engine's link of an import against a **synthetic** module. The cause is readable in `crates/runtime/src/module.rs`: `resolve_export` (`:3015`) walks a module's `local_export_entries`, its `indirect_export_entries` and its star exports, and a synthetic module has none of the three — its names are its host's declaration, in `synthetic.export_names`, which is the list `collect_exported_names` already reads for the namespace (`:2259`). So the import resolves to nothing and linking reports the name as missing. That is the next slice, and it is an engine change: ledger item 15 names it before it is written.

One detail worth recording because it is not obvious: the failure arrives as a `CoreError(Js(JsError { ... }))` with `frames: []` and `source_line: None` — this bridge's own recorded gaps (no call stack, no per-activation positions, §9) showing up in an error a host formats for a user.

**The synthetic-module link — landed, and deno_core's bootstrap completes.** The blocker the previous record named, found by running: linking an import against a synthetic module. The change is the one ledger item 15 named, with the shape its first measurement settled — the module's exports live in a real module environment rather than beside it.

`module_declaration_instantiation`'s synthetic branch now creates what spec 16.2.1.5.2 says a synthetic module's [[Environment]] is: a mutable binding per declared name, initialized to *undefined*. `set_synthetic_module_export` writes that binding (`SetMutableBinding(name, value, true)`) where it used to write a `synthetic.exports` slot table, and `namespace_get` reads it the same way. So that table is gone rather than kept in sync — one storage location, which is what the spec has, and the values are traced through the module's environment (already traced) instead of through a field of their own. `resolve_export` gains the branch it lacked: a declared name resolves to the module itself, bound to that name, which is what the *existing* import-binding machinery (`create_import_binding`) then reads — so an import of a synthetic module's export is live, and no new binding kind was needed.

The creation sits behind the status check that branch already had, and it has to: the branch returns early whether or not the module is linked, so creating unconditionally would discard a host's exports on a second instantiation.

One new engine test, verified by mutating the code it guards — and the first mutation is the diagnosis itself: `resolve_export` answering `None` for a synthetic module reproduces deno's failure exactly, `Module ext:host does not export a`. The second drops the binding write, which the importer then reads as `undefined` (`left: Some(NaN)`).

Gates: `cargo test -p runtime --lib` **795 passed / 0 failed**, `cargo test -p v8 --features simdutf` **204 passed / 0 failed**, `cargo test -p crux --lib` **248 passed / 0 failed**, `cargo clippy --locked --workspace --all-targets -- -D warnings` clean, `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,151 passed / 0 failed / 4 ignored**. `crates/runtime` changed, so the whole battery ran and every number is the certified one: test262 `all` 48,622 — 48,464 pass, 0 fail / 0 crash / 0 hang, 158 skip; `intl402` 3,357 — 3,205 pass, 0 fail / 0 crash / 0 hang, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

**And the bootstrap finished.** Both of `deno/`'s snapshot build scripts now get all the way through deno_core's JavaScript — primordials, `00_infra`, `01_core`, the `ext:core/ops` synthetic module, the extensions' own modules — and report

    Creating a snapshot...
    JsRuntimeForSnapshot prepared, took 231.7474ms

before failing inside `create_blob`, which this bridge aborts *on purpose*: "Slag has no snapshot format: an isolate boots from source" (§9). So the engine runs deno_core's bootstrap now, and what stands between `deno/` and a snapshot is the engine feature §10's engine-side item (3) already names: the snapshot format, with the external references that have to stay index-stable across builds. A host that boots from source needs none of it; deno's CLI does, so that is the next slice and it is an engine one.

**The snapshot format, slice 1 — landed, and `create_blob` produces one.** The engine-side item the record above named, and ledger item 16 names the change before it was written. `crates/runtime/src/snapshot.rs` is the format; `api::Context::write_snapshot` / `read_snapshot` are the two entry points; the bridge's `SnapshotCreator` records and its restore decodes.

*What a blob is.* Not a heap image: a realm here is rebuilt deterministically by `Context::new` on every boot, so what a host has to get back is the state **it** built. The blob is a *value graph rooted at the data the host attached to a context*, written once per value and referred to by serial — so a cycle round-trips as a cycle and two references to one object come back one object. Version 1, with the format version, the pointer width and the byte order in the header and the magic repeated at the tail, so a truncated blob is not a blob.

*Two decisions make the graph small.* A builtin is written **by name, not by structure**: `Intrinsics::name_of_value` (new, in `realm.rs`) answers the name a value is registered under in a realm — functions through the id-keyed name table `name_of` already had, everything else by walking the entry table — so a reference to `%Object.prototype%` is a name that the restore resolves in the realm it is rebuilding, and the walk never descends into a builtin. Without it the first prototype would drag in every builtin the realm has: measured, the alternative is what the *first* failing attempt produced. A value is written once, keyed by identity (an object's id, a symbol's id) or by content where the language makes content the identity (a string's code units, a bigint's hex), and primitives by bit pattern — which is what lets a property's value be a serial rather than a copy.

*The walk refuses rather than guessing*, and names what it refused (`Unsupported { type_name, detail }`): a function body, a proxy, a typed array, a module namespace, a host object, a host's document-all object, a `String` object, an arguments object, an `External`, an array with a hole, an indexed accessor, an array whose length is not an index. The exotic gate is one match over `ObjectKind` in the walk, before the prototype is read, so an exotic kind can never be written as the ordinary object its shape would suggest — a proxy without its traps, a typed array without its buffer, a `String` object without its string. Both the engine walk and the blob writer agree about which values they carry, so nothing is discovered on the read side. `create_blob` turns that error into a panic with the reason, which is the loudest message the crate's `Option` signature allows; the module docs say why a blob that quietly lost part of a host's state would be worse.

The exotic gate was found by asking what the first version would have *done* with a proxy rather than by a failure: it had no `ObjectKind` check, so a proxy's own keys would have been written onto an ordinary object and its traps silently dropped — the exact shape of failure the plan's "a blob that quietly lost part of a host's state" rule forbids. It is therefore in the landed version, with the test above guarding it.

*The index conventions are V8's*, because the host reads them back: context slot 0 is the default context and `AddContext` answers 1, 2, … — `kFirstAddtlContextIndex` in `v8/src/snapshot/snapshot.cc`, which the bridge's earlier 0-based answer did not match. Nothing depended on the old value, since no blob existed to read it back out of. A context's own data starts at 0 and each attached item answers the next index. `StartupData::is_valid` is now a real question — magic, version, word size, byte order, lengths, tail — and an isolate handed a blob that is not this engine's **boots from source** rather than consuming it, which is what the host's own `is_valid` check told it to expect.

*The slot table is the engine's, not the bridge's, and that is a measurement rather than a preference.* The first version had the bridge build an array-of-item-arrays with `api::Array::new`, and it failed: that path creates the array through the agent's **current** realm (`builtins::array::create` → `agent.current_realm()`), so with a second context created — which is exactly what `AddContext` does — the root array carried the *other* realm's `%Array.prototype%`, the walk could not name it in the realm it was writing, and it descended into that realm's builtin methods until it hit a function. `encode_slots` therefore builds the table in the realm it was given, via `realm.intrinsics.array_prototype()`. What is still not carried, and says so where a host would look for it: data attached to a context other than the default one refuses at `AddContextData` with "an isolate here has one realm" — a second `Context::new` shadows the first in this engine's api, so a second realm's objects are built in the wrong one and a blob of them would be *wrong* rather than partial. *(Superseded by the next record: the table carries a slot per context and that refusal is gone.)* Isolate-level data answers `NoData`, as its doc says. Continuation from an existing blob is not consumed yet. *(And the external references this record lists as missing land two records on.)*

*Both modes write the same blob*: nothing carries a function body, so no compiled code is carried in either, and `FunctionCodeHandling` is recorded rather than honored. The code cache and the unbound scripts (§12 item 3) are what that waits on.

*Tests.* Sixteen in `crates/runtime/src/snapshot.rs`, end to end over the format: primitives, a lone surrogate (code units, not a `String`), bigints by value, well-known symbols keeping their identity, registry symbols coming back as the registry's own, two symbols with one description staying two, an object's attributes and its intrinsic prototype by name, a cycle, a null prototype, an array with its length and elements, a hole refused, each exotic kind refused by name, a root that names no record, and a foreign blob refused by each of its header fields. Sixteen in `crates/v8/snapshot.rs`, through the V8-shaped surface: data attached and read back at its index, a self-reference surviving, an index read once, a slot the blob does not name answering `None`, an isolate without a blob having no data, a foreign blob not consumed, a function ending the build with its name, the slot convention, a creator without a context, data for another context refused, and a restored value made persistent and read after its scope is gone. Four were verified by mutating the code they guard and restoring it: passing `None` for a decoded prototype fails the prototype test; a refusal whose name is not the one the walk uses fails the exotic test; writing a symbol without its well-known entry fails the identity test; taking a snapshot item without removing it fails the read-once test.

One thing did not need a test because it could not fail: a blob written by this tree is one this tree reads, asserted as the positive control in every round-trip test.

*And the checkpoint moved under this work, in our favor.* `deno/`'s root now aliases the `v8` dependency to a facade: `Cargo.toml:109` declares `v8 = { package = "deno_v8", path = "./libs/deno_v8", default-features = false, features = ["simdutf"] }`, and `libs/deno_v8` ("JavaScript engine facade for Deno") depends on `rusty_v8 = { package = "v8", version = "150.4.0", optional = true }` — which the root's `[patch.crates-io] v8 = { path = "../crates/v8" }` redirects to this crate. `deno_core`'s `default = ["v8", ...]` turns that on, so **`cargo check -p deno_core` answering 0 is measured through the facade into `crates/v8`** — the hijack is no longer a plan, it is what the checkpoint's dependency graph does.

What that means for the next measurement, and why it was not taken here: re-running the snapshot build script (`dcore`, or `libs/core/examples/snapshot`) fails *before* deno_core's JavaScript runs, because `dcore`'s **build-dependency** graph (`deno_core_testing`) resolves the facade without a backend under `resolver = "2"`'s per-kind unification, and `deno_v8` is a `compile_error!` then. That is the checkpoint's own feature selection rather than this change, and it is the same trap §10 records for `-p` measurements: the load-bearing number is `-p deno_core`, and it is 0. The bootstrap's next blocker past `create_blob` is therefore not measured yet, and the named candidate is the one §10 lists: deno attaches its data to the realm it added at slot 1.

Gates: `cargo fmt --all -- --check` clean; `cargo test -p runtime --lib` **811 passed / 0 failed**; `cargo test -p v8 --features simdutf` **213 passed / 0 failed**; `cargo test -p crux --lib` 248 / 0; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,176 passed / 0 failed**. `crates/runtime` changed, so the whole battery ran and every number is the certified one: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail. Nothing in this workspace depends on `crates/v8`, so the bridge half implicates no sweep.

The battery ran on the tree as it stood when the format landed; the exotic gate and the `visit` signature came after it, and both are inside `crates/runtime/src/snapshot.rs`, so the numbers still stand for this tree — checked by grep rather than assumed: the only files naming `write_snapshot`, `read_snapshot`, `snapshot::encode` or `Intrinsics::name_of_value` are `crates/runtime/src/{snapshot.rs,realm.rs,api/context.rs}` and `crates/v8/snapshot.rs`, and none of `crates/test262`, `crates/wasmtest`, `crates/wasm` or `crates/cli` names `runtime::api` at all.

**The context table, one slot per context — landed, and the one-realm refusal is gone.** The gap the record above named, and the half `deno_core` actually reads: its build takes a *fresh, empty* context as slot 0 (`jsruntime.rs:2937`, `v8::Context::new` then `set_default_context`) and adds the realm it bootstrapped in at slot 1, attaching everything it snapshotted to *that* context. So slot 1 is not a corner case, it is the only slot with data in it.

*The table is structural now, not a value graph.* Slice 1 built the table as an array of arrays — engine values, which is what made the realm question unavoidable: the array had to be made in *some* realm, and the first version's failure came from making it through the isolate's current one. A table is the format's own structure, so it is written as one: the header carries the context count and the body carries `(slot, item count, item serials)` per context, then the records. No scaffolding objects exist to be attributed to a realm.

*Each slot is written and read against its own realm.* `encode_slots` takes `Slot { index, realm, items }` and walks each slot's items with that slot's realm; `decode_slot(realm, bytes, slot)` materializes that slot's items in the realm being restored into. The realm is not a hint: a value's builtins are its own realm's, and a reference to one is written as *the name its realm knows it by*. So every record remembers the realm it was first reached through — that is the realm whose `name_of_value` recognizes it — which is why `objects` carries `(Value, Handle<Realm>)` pairs rather than values.

*An intrinsic name is realm-agnostic, and that is the mechanism rather than a leak.* Two realms' `%Object.prototype%` are two objects with one name, so they share the identity key, share one record, and each restore resolves the name to the realm it is materializing in — which is the correct object for each slot. A value *shared* between two slots that is not an intrinsic comes back as one value per slot, because a restore makes values one slot at a time; a host that needs one object in both realms has a cross-realm reference, which V8's own snapshot would carry and this format does not claim to yet.

*And a value built in the wrong realm now says so.* The slice-1 measurement chased that failure by hand: an object from realm B written into a slot named by realm A mis-recognizes B's `%Object.prototype%`, descends into it, and surfaces as "a function". `uncarried_callable` checks the agent's other realms before answering, so the refusal is now `a value from another realm` with the reason — a host's mistake about contexts, which has a different fix from a missing feature. It costs one scan of the realm list, on the failure path only.

*The bridge stopped refusing.* `add_context_data` accepts an added context and answers the index within that context's own list; `slot_of` is default → 0, added → 1, 2, … again; `create_blob` writes one table entry per recorded context (empty lists included, because "recorded with nothing" is not "not named"); `restore` decodes the slot the host asks for, in the realm `from_snapshot` just created. The one remaining refusal is for a context the creator never recorded, which is V8's own check and now says which call to make first.

The version stays 1: nothing outside this tree has ever written a blob in this format, so a layout change is part of v1 rather than a v2 — the header's version field is what will make the next change a v2.

*And the next blocker is now named.* With the table in place, deno's `create_blob` no longer fails for lack of a slot — it fails on what deno *registers*: `SnapshotStoreDataStore::register` takes `v8::Global<Function>`s (the promise-rejection callback, the `ext_import_meta_proto` object) and function templates, and a function body is the one thing this format refuses by name. So carrying a function is the next slice, and it splits the way §12 item 3 already splits it: a JS body recompiled from the source the engine kept, and a *native* one — an op — named through the index-stable external-reference table, which is the other half of engine item (3).

*Tests.* Three in the engine: two slots keeping their own items with a third slot answering `None`; a slot written and read against its own realm, asserted on the restored prototype's *identity* (the second realm's `%Object.prototype%`, not the first's); and a value built in another realm refused as such. Two in the bridge, and the second is the acceptance test for this slice: `an_added_context_carries_its_own_data` — an empty-item default at slot 0, an object built in the added context's realm at slot 1, each restored into its own context and read back. Two mutations, both caught: writing every slot against slot 0's realm fails the added-context test with the engine's refusal at `create_blob`, and making `uncarried_callable` always answer "a function" fails the refinement test.

Gates: `cargo fmt --all -- --check` clean; `cargo test -p runtime --lib` **814 passed / 0 failed**; `cargo test -p v8 --features simdutf` **214 passed / 0 failed**; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,180 passed / 0 failed**. `crates/runtime` changed, so the battery ran again and every number is unchanged: test262 `all` 48,464 pass / 0 fail / 0 crash / 0 hang (158 skip), `intl402` 3,205 pass / 0 fail (152 skip), the eight wasm core suites 64,594 checks / 0 fail / 0 pending, the JS-API sweep 1,001 tests / 0 fail.

**External references — landed, and a host pointer now round trips.** The other half of the duo engine item (3) has carried since the plan was written, and the piece `crates/v8/external_references.rs` has promised in its own header from the start: *"a snapshot cannot hold a function address: the address is a property of the process that loads it, not of the data, so a snapshot names an **index** into a table of these instead, and the embedder rebuilds the table for every load."* That is now what happens.

*The shape.* `encode_slots(agent, slots, externals)` and `decode_slot(agent, realm, bytes, slot, externals)` take the table; an `External` is a record holding an index and nothing else — no address, in either direction. A pointer the build's table does not have refuses by name (`a host pointer`, "the pointer is not in the external-reference table"); an index the load's table does not have refuses with the index and the table's length (`ExternalIndexOutOfRange`), which is a read V8 would have made past the end. The engine keeps no table of its own: the table is the *host's* argument, which is the whole point of an index.

*And the bridge stopped ignoring its own table.* `CreateParams::external_references` was carried and unused; it is now the table a restored isolate resolves against (`IsolateInner::externals`), and the creator's constructor argument is the table `create_blob` writes indices into. `snapshot::addresses` reads the union's pointer field to build the engine's view, and `references.rs`'s own test — every field of the union is one pointer — is what makes that read the entry rather than a guess.

*A restore that fails after `is_valid` passed now panics with the reason* instead of answering `None`. A table that does not match the blob is a host contract violation, and the crate we stand in for's own loader checks in that situation; answering `None` would send the host down its "boot from source" branch with no way to learn why. `None` still means exactly one thing — the blob names no such context slot — and the docs say so.

*Two decode errors got precise at the same time*, because the builder had to learn to report them at all: a blob naming an intrinsic the reading realm does not have is `UnknownIntrinsic(name)`, and an index past the table is `ExternalIndexOutOfRange { index, count }`, where both used to surface as a generic `Truncated` — "the blob is broken" for a blob that was fine and a host that was not.

*What it does not do, and the next slice is named.* It carries *pointers*, not the things they point at. deno's `Global<FunctionTemplate>` values are `External`s naming a template address in a bridge-owned `Rc`; a blob now gives the same address back, but what that address *means* at restore is the host's contract (the restoring isolate's template list), and deno's restore path reads its templates out of the blob rather than re-creating them. And a function is still refused, for two different reasons: a JS function needs the scope it was compiled in (source plus the environment it closed over — the module env for anything deno's bootstrap defines), and a host function's callback in this bridge is a Rust closure, not a function pointer a table could name. The `function` field is in the union already, which is where `#[op2]`-style ops would land.

*Tests, and one that could not fail.* Three in the engine: a pointer round tripping both as a root and nested in a property; a pointer absent from the table refused by name; and the index contract — an out-of-range index refused with the count, and, on a two-entry table, the restored address being *that index's* entry rather than the blob's. Three in the bridge: the round trip through the creator's table and the restored isolate's rebuilt one, a pointer absent from the creator's table ending the build, and a table the blob does not match ending the restore. Two mutations: answering index 0 for an absent pointer fails the refusal test, and reading the table's first entry instead of the index fails the bounds test.

That second mutation is worth recording because it *did not fail* the first version of the test: with a one-entry table, an ignored index and a used index are the same read. The test now uses a two-entry table with the pointer at index 1, where they are not — which is the general shape of the trap the plan's "a test that cannot fail" rule is about, caught here by mutating rather than by inspection.

Gates: `cargo fmt --all -- --check` clean; `cargo test -p runtime --lib` **817 passed / 0 failed**; `cargo test -p v8 --features simdutf` **217 passed / 0 failed**; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,186 passed / 0 failed**. `crates/runtime` changed, so the battery ran and every number is unchanged: test262 `all` 48,464 pass / 0 fail / 0 crash / 0 hang (158 skip), `intl402` 3,205 pass / 0 fail (152 skip), the eight wasm core suites 64,594 checks / 0 fail / 0 pending, the JS-API sweep 1,001 tests / 0 fail.

**Carrying a function — landed, and deno's `create_blob` moves one kind further.** The blocker the record above named, and the fourth part of ledger item 16. A JavaScript function is carried as the **source text** it can be re-parsed from, its [[Strict]], and its object part, and the restore evaluates that source in the reading realm's global environment.

*What a record is.* `REC_FUNCTION` (tag 14): a strict byte, the source as UTF-16 code units, the prototype serial, extensible, and the same property list an object gets — a function's own keys are its own keys, so `length`, `name` and a host's own property ride along rather than being recomputed. The walk descends into the function's object part, so a `prototype` object a host filled in is carried by the graph like any other.

*Why source and not a body.* A compiled body is not something a blob can name: it is an `Rc<CompiledBody>` over literal `Value`s and step indices, and the engine rebuilds it deterministically from an AST it can parse. So the record holds what a parser can read, and `instantiate_function_from_source` (new in `function.rs`) is the one door into it: `parser::parse_function`, the async form as the single retry (an `async function` source has no standalone expression entry in `parse_function`), then OrdinaryFunctionCreate with the record's strictness. That last part is why the helper exists instead of reusing `instantiate_dynamic_function` — a function that is strict because of its *enclosing* context has no `"use strict"` of its own, and re-deriving strictness from the source alone would silently restore it sloppy.

*The environment is the limit, and it is stated rather than papered over.* A blob carries the value graph, not the environment chain a closure was compiled in, so a restored function resolves a free name through the **reading** realm's global environment. That is enough for a self-contained body and not for a closure over a module's bindings, and the module docs say so where a host will read it. The walk refuses what source cannot express at all, by kind: a **host callback** — a `FunctionKind::Builtin` that is not an intrinsic, which in this engine is a Rust closure rather than an address or a name a table could hold — a **bound function**, and a function the engine kept no source for (an arrow, a method, an accessor; `Function.prototype.toString` already answers the native form for them).

*Two things the tests found, not the design.* A function reached as an **object** — `Object.setPrototypeOf(x, f)` stores `f`'s object side, and the engine keeps a `function_self` back-reference on it for exactly that — had to be folded back into the function value before a serial is assigned, or the walk would write an ordinary object where a function belongs; `canonical` does it at the one place that decides what a serial names. And `%Function.prototype%` is **callable**, so the reader's `prototype()` could not use `Value::as_object` — that accessor answers `None` for a function value, and the resumable kinds' prototypes are functions too — and uses the engine's own `as_object`, the same coercion the builtins use. The first was caught by the test written for it and the second by every function test failing as "the record names no prototype" on the first run.

*And the deferred `prototype` needed a rule.* A plain function's `prototype` is created lazily, and the record carries it only when the writing realm had materialized it. Clearing the deferral flag on every restore leaves a function with no `prototype` at all; leaving it set when the record *does* carry one makes the first observation that crosses the barrier — a descriptor read, `new`, own-name enumeration — append a **second** `prototype` beside the restored one, measured as the own-key count going 5 to 6. So the reader clears the flag exactly when the record carries the property, and each half of that condition has a test.

*Tests.* Eight in `crates/runtime/src/snapshot.rs`: a function round trips and is callable, and a deferred `prototype` still materializes on an observation; its own properties ride with it; a free name resolves through the **reading** realm's global, measured across two realms (10 in the writer, 100 in the reader, the answer 105); two items of one slot come back one function; a function reached as a prototype comes back a function; a materialized `prototype` survives and stays single; each uncarried kind is refused by its own name; and a host callback is refused as a Rust closure. Two in `crates/v8/snapshot.rs`: the acceptance test — a `v8::Function` attached to a context, restored, and called from a script (`restored(41)` answering 42) — and the honesty test, a `FunctionTemplate`'s function still ending the build by name.

Each of the eight engine tests was verified by mutating the code it guards and restoring it: disabling `canonical` fails the prototype test; dropping the method refusal writes the method's source (`m() { return 1; }`, which is not a function expression, and the failing blob shows it); making the builtin arm fall through fails the callback test; removing the `prototype_pending` clear fails the duplicate count; clearing it unconditionally fails the deferred-prototype test with the same `null` V8 gives a function with no `prototype`; removing the bootstrap execution context fails with "No running execution context"; discarding the record's properties fails on a missing `name`; and instantiating in the *first* realm instead of the reading one answers 15 instead of 105. The shared-function test was written against an array first and **passed** under a per-occurrence mutation — an array's elements are written from the serial map, which a mutation that overrides keys masks — so it was rewritten as two items of one slot, where the mutation is caught. That is the second time this plan's "a test that cannot fail" rule has been caught by mutating rather than by inspection, and it cost a rewrite rather than a wrong claim.

Gates: `cargo fmt --all -- --check` clean; `cargo test -p runtime --lib` **825 passed / 0 failed**; `cargo test -p v8 --features simdutf` **218 passed / 0 failed**; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,195 passed / 0 failed / 4 ignored**. `crates/runtime` changed, so the battery ran again and every number is the certified one: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail. Nothing in this workspace depends on `crates/v8`, so the bridge half implicates no sweep.

*And the checkpoint moved again, by one kind.* `cargo check -p deno_core` still answers 0. The example that builds its own snapshot cannot be built through its own build-dependency graph, and that is the checkpoint's feature selection rather than this change: `deno_core` is a *workspace dependency* with `default-features = false` and a feature list without `v8`, so the facade is a `compile_error!` when `deno_core` is used as a dependency rather than as the `-p` root — `--features deno_core/v8` supplies it for a measurement. With that, the build script runs deno's bootstrap to the end (`JsRuntimeForSnapshot prepared, took ~253ms`) and fails inside `create_blob` on the **next** kind rather than on a function: *slot 1, item 0 — the module map deno serializes for snapshotting — reaches a **bound function***. So what the record before this one left refusing is refused no longer — a `v8::Function` attachment round trips and is callable, through the bridge's own tests rather than through deno's graph, which stops before it reaches one — and what stands between `deno/` and a blob is one more kind, whose shape the refusal already names: a bound function is its target plus its bound `this` and arguments, and a target that is itself uncarried still refuses. That is ledger item 16's fifth part and §12 item 2's next slice.

**Bound functions — landed, and deno's `create_blob` now reaches the realm's global object.** The fifth part of ledger item 16, and the kind the record above was measured stopping on.

*A bind is three values and an object part.* `REC_BOUND_FUNCTION` (tag 15) carries the target, the bound `this`, the bound arguments, and the same prototype/extensible/properties triple a function gets. The walk descends into all four — the three values as ordinary references, so a chain of binds round trips and an intrinsic target is still written by name. The restore calls `Function::bound_function_create`, the crux constructor the `bind` builtin itself uses, and then defines the record's own properties. Nothing is recomputed from the target: a bound function's `name` (`bound add`) and `length` (the target's 2 less the 1 bound argument) are own properties, and they come back as the record wrote them.

*One file, and no new engine surface.* `bound_function_create` was already public and takes no agent, so this is `crates/runtime/src/snapshot.rs` alone. The walk's decision by kind became an explicit `Callable` enum (`Body { source, strict }` or `Bound { .. }`) rather than a helper returning the body record, because a bind has no body to return and the writer needs the same decision the walk made. A bind whose *target* is a host callback refuses by the **target's** name — the value that cannot be carried, not the binding around it — which a test now pins.

*Tests.* Four in the engine: a bound function round trips and is callable, with its `length` and `name` read back off the record; a bound receiver and a bound argument are objects and come back objects (the answer is `this.tag + a.n + b`); a chain of two binds reached **through an object** round trips — the shape deno's value arrives in; and a bind over a host callback is refused by name. One in the bridge: `restored(2)` answering 3 from a bound function attached as context data. Four mutations, each caught: routing the `Bound` kind into the body lookup (all three bound tests fail with "its body is not registered on this agent"), writing `NO_REF` as the target, dropping the bound arguments at restore (the call answers `NaN`), and not walking the bound `this` (the restore refuses as truncated). The refusal test lost its bind case at the same time, because the case is positive coverage now.

Gates: `cargo fmt --all -- --check` clean; `cargo test -p runtime --lib` **829 passed / 0 failed**; `cargo test -p v8 --features simdutf` **219 passed / 0 failed**; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,200 passed / 0 failed / 4 ignored**. `crates/runtime` changed, so the battery ran and every number is the certified one: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

*And the next blocker is a nameable builtin, not a callback.* Re-measured the same way, `create_blob` now gets through the bind and stops on `isFinite` — a **standard global function the realm installs but does not name**. The engine's `builtins/global.rs` puts the eight global function properties (plus `escape`/`unescape`) straight on the global object, while `%eval%` — the same kind of thing — is registered in the intrinsic table (`realm.rs:509`), which is the mechanism the next step reuses.

*Two things a probe settled, both of which change what the step after that is.* The refusing builtin is **not** a bind's target, so binds are genuinely carried now rather than being walked into their targets. And the walk **reaches the realm's global object** before it refuses: deno's attached graph holds a path to `globalThis`, so the blob now wants to carry every global the host installed during its bootstrap, of which the engine's own are nameable and the host's are ordinary values that would come back on top of a realm the restore rebuilt. That is the shape of the next slice's real question, and it is measured rather than inferred — the probe printed both facts and was removed again.

*Ledger item 16's sixth part named the naming step before it was written: a `%name%` entry for each of the realm's global function properties, so a reference is written by name and resolved in the reading realm that rebuilds them, and no callback is carried for them. It lands in the record below, which is also where the corpus caught what registering them exposed.*

**The realm's global function properties — landed, and deno's `create_blob` now stops on a class constructor.** The sixth part of ledger item 16, and the naming step the record above measured.

*The spec names them, so the registry is the right home.* `builtins/global.rs::install` created the global function properties as builtins on the global object and never registered them, so `isFinite` was a built-in the walk could not name. It now registers each one under the spec's own well-known name — `%isFinite%`, `%isNaN%`, `%parseFloat%`, `%parseInt%`, `%encodeURI%`, `%encodeURIComponent%`, `%decodeURI%`, `%decodeURIComponent%`, and Annex B's `%escape%`/`%unescape%` — in the same table `%eval%` (`realm.rs:509`) already sits in. The names are the spec's, not invented: `spec.html` lists every one in the well-known intrinsics table and defines each as an intrinsic object ("This function is the %isFinite% intrinsic object", "It is the %escape% intrinsic object"), and the table's own documentation ("the intrinsic registry (spec 9.3.1): %-named values installed by each built-in phase") is exactly what these are. Nothing in the format moved: `REC_INTRINSIC` already carries a name, and the restore resolves it in the realm being rebuilt.

*Tests.* One in the engine, over all ten names: the name is in the blob, and a restore **into a second realm** answers that realm's own function rather than the writer's — the strong form, since a copy would carry the writer's identity. (The reading realm is created *after* the value is in hand, because the api makes the last context created current and the evals have to run in the writer's; the reader's own function is read from its intrinsic table rather than by an eval for the same reason.) One in the bridge: `isFinite` attached as context data, called from a script after the restore. Two mutations: dropping the registration fails the ten-name test with the refusal this slice removes ("a built-in function"), and misspelling one name fails the *blob-content* assertion — the one that pins the name, which is the format's compatibility surface, since a name registered on both sides would otherwise round trip under any spelling.

*And the corpus caught an interaction the engine had been hiding.* Registering these made `owning_realm` (which finds a builtin's realm by scanning a realm's intrinsic table) answer for ten more functions — a **more correct** GetFunctionRealm — and that exposed an over-broad path in `construct_inner`: its cross-realm block converted *any* kind-only error from a cross-realm callee into that callee's realm, including EvaluateNew's "not a constructor" TypeError, which spec 13.3.5.1.1 step 7 throws in the **caller's** realm. Measured rather than reasoned about: `language/expressions/new/non-ctor-err-realm.js` went 0 fail to 1 fail — "Expected a TypeError but got a TypeError", from `assert.throws(TypeError, ...)` over `new $262.createRealm().global.parseInt(0)`, which compares the thrown object against the *test's* `TypeError`. The fix gates the block on `is_constructor`: a callee that will never run has no realm to attribute an error to, and only the dispatch half of the block (running the constructor in its own realm) needs the callee at all. That also makes the same shape correct for a builtin that was *already* registered — `new otherRealm.Symbol()`, which used to get the callee's TypeError with no fixture pinning either answer.

One engine test pins the boundary (`function.rs`), mutation-verified: with the guard removed it answers `false` where it should answer `true`. The battery re-ran after the fix and every number is unchanged.

*And the message for the next value was wrong.* The refusal for the value deno now stops on reads "a method (a method's source has no `function` keyword, ...)", and a probe showed what the value actually is: a **class constructor** — `is_class_constructor` true, no source at all, a home object, no name. So the detail was asserting a fact about a different value's shape: a class constructor is refused because the engine kept no source text for it (its body is synthesized), not because a `function` keyword is missing. The check order is now source-first, and a class constructor is named as one. One engine test, mutation-verified by putting the method check back first — only that test fails, with `left: "a method"`.

Gates: `cargo fmt --all -- --check` clean; `cargo test -p runtime --lib` **832 passed / 0 failed**; `cargo test -p v8 --features simdutf` **220 passed / 0 failed**; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,204 passed / 0 failed / 4 ignored**. `crates/runtime` changed, so the battery ran — and ran twice, because the first run *failed*: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

*What is left, and it was a design question this time.* `create_blob` got through the global object's functions and stopped on that **class constructor** — a value whose body the engine synthesizes, so there was no source to re-parse and no class constructor in the engine had one. Ledger item 16's seventh part took it from there, and it was the first of these steps whose answer was not "write one more record": what the probes said the value is (a base class, four instance fields, a private environment, an implicit constructor, no methods) is written up there with the design, the alternative that was rejected and why, and the definition-time divergence the chosen one accepts.

**Class constructors — landed, and deno's `create_blob` now stops on a host callback.** The seventh part of ledger item 16, and the design that paragraph named.

*The record carries the spec's `[[SourceText]]`, and the restore re-runs the class.* `REC_FUNCTION` gains a **grammar byte** after its tag — `function`-expression or `class`-expression — because the value kind is the same and only the text's grammar differs; the strict byte, the prototype, extensible and the property tail are written and read by the same code either way. The engine half is one capture: `class.rs`'s ClassDefinitionEvaluation takes `capture_source(agent, class.span)` — the **class** text, which is what the spec sets for an implicit constructor and an explicit one alike — and threads it through `instantiate_class_constructor` / `instantiate_class_constructor_with` / `instantiate_default_derived_constructor` as a new `source: Option<JsString>`, for both branches. The body-cache key is unchanged: the explicit branch still keys `shared_function_body` on the constructor *method*'s span, and the class source reaches only the constructor's own record.

The restore's `Builder::build_class_function` evaluates `(<class source>)` as a **class expression** in the reading realm — the same bootstrap execution context the function path pushes, popped on every path — takes the completion value as the constructor, sets the record's prototype over the one the evaluation derived, and then defines the record's properties exactly as a function does. Everything a class constructor is made of therefore comes back from the engine's own ClassDefinitionEvaluation: its `[[ConstructorKind]]`, home object, prototype object, field records, private names and their environment, and `[[IsClassConstructor]]`. That is also the cost, stated in the module docs: definition-time code — a computed key, a `static {}` block, a static field initializer — **runs again** at restore, where V8's snapshot only restores objects.

*Two consequences of the capture, one of them conformance.* A class constructor's `Function.prototype.toString` now answers the class source, which is what the spec requires and what the engine answered the native form for; the four `builtins/Function/prototype/toString/class-*-ctor.js` fixtures cannot see the difference (`assertToStringOrNativeFunction` accepts either branch), so a fresh class is pinned by a test of ours instead. And the **implicit** constructor has a source for the first time — the measured deno class is exactly that case.

*Tests.* Four in `crates/runtime/src/snapshot.rs`: a class constructor round trips, constructs (`new restored().x` answering the field's 7), refuses a bare call with a `TypeError` (so `[[IsClassConstructor]]` came back), and answers the class source from `toString`; a class whose public field reads its own **private** field (`#secret = 41; x = this.#secret + 1`) answers 42, which proves the private environment *and* the initializer were rebuilt and needs no method to observe; a **fresh** class's `toString` answers its class source, the conformance half above; and a class the engine kept no source for is still refused as a class constructor. One in `crates/v8/snapshot.rs`: a class attached as context data, restored, and constructed from a script — `new restored().x` answering 7 and `restored.prototype.constructor === restored`.

The refusal test is the previous record's `a_class_constructor_is_refused_by_what_it_lacks`, **re-aimed rather than deleted**: the branch is still reachable, so it now uses a class defined where the enclosing execution context has no source text — `(new Function('return class C { x = 1; }'))()` — and asserts the same "a class constructor … no source text" pair. Without the re-aim the branch would have had no coverage at all.

*Mutations, and one honest limit.* Writing `Grammar::Function` for a class fails both class round-trip tests with `its source does not parse as a function expression: SyntaxError: Unexpected token` — a wrong grammar byte is a wrong record, not a subtle one. Dropping the `class_source` capture fails three tests, and the fresh-class `toString` test fails on exactly the conformance gap: `left: Some("function C() { [native code] }")`, `right: Some("class C { x = 7; }")`. The third named mutation — the class branch evaluating without the reading realm's environment — is **not independently observable in this harness**, and that is a result rather than a pass: the api makes the last context created current and every test has to eval in the reading realm, so the same mutation applied to the function record's own two-realm test is equally invisible. What *is* caught is the other half of the same discipline: pushing nothing while still popping leaves the agent with no running execution context, and `run_restored` fails with `ReferenceError: No running execution context`.

Gates: `cargo fmt --all -- --check` clean; `cargo test -p runtime --lib` **835 passed / 0 failed**; `cargo test -p v8 --features simdutf --lib -- --skip the_data_a_built_function_carries_survives_a_collection` **220 passed / 0 failed / 1 filtered**; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,208 passed / 0 failed / 4 ignored**. `crates/runtime` changed, so the battery ran and every number is the certified one: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

*A class with methods is still not carried*, which is the eighth part: the walk reaches the prototype object's method properties and refuses them as methods, whose source is method-form and needs a parse context of its own plus its carried `[[HomeObject]]`. The measured deno class has none — its prototype object's own keys are exactly `["constructor"]` — which is why this step is one part rather than two.

*And the next blocker is the one this bridge's own docs name.* Re-measured the same way, `create_blob` gets past the class and stops on a **host callback**: `a built-in function (a host callback is a Rust closure, not a name or an address a snapshot can carry)` — a `FunctionKind::Builtin` with **no internal name and no own `name` property**, which is a probe's answer rather than a guess, and the probe is removed. That is the `function` field of `v8::ExternalReference`, the field `crates/v8/snapshot.rs` has called "not wired to real ops yet" since the format landed and the one `deno/libs/core/runtime/bindings.rs:63` opens with `call_console`, `import_meta_resolve`, `catch_dynamic_import_promise_error` and `op_disabled_fn` before the callsite helpers and the per-op entries. `call_console` is installed as `Deno.core.callConsole` by `v8::Function::builder(call_console).build(scope)` (`bindings.rs:363-367`) — a builder-made function with no name at all, which is the shape the probe found — and which one it is, the next record's first probe names. The mechanism is the one already in the tree: a host pointer is written **by index** into the host's table and resolved on load, so a callback is the same index with a different restore. §12 item 2's next step.

**Host callbacks — landed, and deno's `create_blob` now carries `Deno.core.callConsole`.** The ninth part of ledger item 16, designed before it was written, and the value the paragraph above measured.

*A builtin is a Rust closure, so the host has to answer.* `FunctionKind::Builtin` is a `Box<dyn Fn>` — not a name, not an address the engine could look up — so this is the first kind the engine cannot carry alone, and the record is a *pair*: the index of the host's external-reference table entry the function was built from, and the **data** value it reads. The index is the mechanism the format already uses for a host pointer; the data is a value, because deno builds every op with one (`.data(external.into())`, `bindings.rs:1008`), so a record that dropped it would restore a callback that runs a different branch of the host's code.

*The engine side is one record and two host-facing parameters.* `REC_HOST_CALLBACK` (tag 16) carries the index, the data serial, and the same prototype/extensible/properties tail a function gets. `callable()` gains an `Option<&dyn HostCallbacks>`: a builtin the host recognizes becomes `Callable::HostCallback { pointer, data }`, whose data the walk visits like any other value, and a builtin it does not becomes exactly the "a built-in function … a Rust closure" refusal it always was. `encode_slots`/`decode_slot` and `api::Context::write_snapshot`/`read_snapshot` take the host as a parameter — **no `Agent` state and no `crux` change**, so the format's own tests and the engine's refusal test are untouched — and the restore builds the function through the engine's own `api::template::host_function` (now `pub(crate)`, one word), so a restored callback's call view, return-value slot and pending-exception translation are the engine's existing ones rather than a second implementation.

*Two things are refused rather than carried, each for a reason the record cannot answer.* A builtin the host recognizes as **constructible** is refused as *a host constructor*: its `[[Construct]]` is the template the host built it with — the instance it makes, the instance template it applies, what a non-object return value means — and a blob carries no template. Deno sets that per op (`op_ctx_constructor_behavior`, `bindings.rs:800`), so the ops and `call_console` are the kind this carries, and a constructable op would be refused by name rather than restored without `new`. And a callback whose pointer the **build's** table does not hold refuses with the host-pointer message, because an index nothing would resolve is worse than a build that says which table entry to add.

*The bridge had to learn what it built.* `crates/v8`'s isolate records, per materialized function, the callback address and the data its template carried (`BuiltCallback`): populated in `FunctionTemplate::from_parts` against the template's address, moved onto each function `get_function` makes. The address is stable because `map_fn_to()` monomorphizes the adapter per host function type — the same `call_console.map_fn_to()` is both the entry deno registers and the callback the function was built with — and the data is held pinned, because the walk reads it from the isolate's table rather than through the closure the function calls through.

*Tests.* Six in `crates/runtime/src/snapshot.rs`, through a fake host the test owns (a `HostCallbacks` impl over builtins the test made and a pointer map): the round trip, taking the **index's** pointer back out of a two-entry table and coming back callable; a bind whose target is that callback, which is the shape deno's first measurement was; the data riding with it; a pointer absent from the table refused by name; a constructible callback refused as a host constructor; and a blob naming a callback read by a host-less load refused as `NoHostCallback`. Two in `crates/v8/snapshot.rs`: the acceptance test — a template's function with `ConstructorBehavior::Throw` and a `.data(...)` value attached as context data, restored, and called from a script, answering the data — and the test that used to end the build on a callback, re-aimed at the *table* refusal it now is.

Seven mutations, each caught: index 0 instead of the entry's, the host not consulted, the record's prototype dropped at restore, the record's own properties dropped, and the data written as `NO_REF` / ignored at restore (both of which fail the engine's data test and the bridge test).

Gates: `cargo fmt --all -- --check` clean; `cargo test -p runtime --lib` **840 passed / 0 failed**; `cargo test -p v8 --features simdutf --lib -- --skip the_data_a_built_function_carries_survives_a_collection` **221 passed / 0 failed / 1 filtered**; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,214 passed / 0 failed / 4 ignored**. One workspace run before those showed a single failure; two re-runs were clean and the test is the documented flake `certified_body_global_read_fast_path_stays_spec_exact`, which passes on its own. `crates/runtime` changed, so the battery ran and every number is the certified one: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

*And the measurement is that deno's build moved by one value and stopped on a fact about the host.* `create_blob` **carries `Deno.core.callConsole`** — the probe printed `carried index=0`, and `bindings.rs:63` is entry 0, so the callback the paragraph above stopped on is the one this record carries — and then refuses the next callback's pointer as absent from the host's **426-entry** table. That is a host-side gap rather than a missing record: deno builds those callbacks during the bootstrap's module evaluation (`Function::builder(closure).data(...)` in `deno/libs/core/modules/map/{evaluation,dynamic}.rs`) and `create_external_references` never lists them. Whether V8 requires them to be listed, or encodes an unknown callback as a null reference and lets the host replace it, is **not** in this checkout — that is the one question this measurement leaves open, and it is recorded rather than guessed. What is left on this record's own terms: the construct half, and the entry-retirement question §12 item 7 already records for the bridge's position table, which the new function→callback table shares.

**Arrows and the bridge's own console — landed, and deno's build stops on a function the *engine* left without a source.** The tenth part of ledger item 16 and one bridge-side fix, and between them they moved deno's build past two more values.

*An arrow had no `[[SourceText]]`, and the spec says it has one.* `instantiate_arrow` built its record with `source: None`, so an arrow's `Function.prototype.toString` answered the native form and a snapshot had nothing to carry it by. The capture is the fix: the arrow's span travels to the instantiation — through `Step::CreateArrow`, which carries the span because the AST node is decomposed into params and body there, and through the interpreter's and the JIT's helpers — and `capture_source` stores the text. The engine's own `function_to_string_renders_source_or_native` **pinned the gap** (it asserted `function g() { [native code] }` for an arrow) and now asserts `(x) => x`; a test that has to change to keep passing is the clearest evidence a change is a conformance fix rather than a preference.

*The record carries it as an expression.* `GRAMMAR_ARROW` (byte 2), and the restore evaluates `(<source>)` in the reading realm rather than parsing it, because `parse_function` wants a `function` keyword and an arrow has none — the class record's path, so `build_class_function` became `build_evaluated_function(source, proto, grammar)` and serves both. `[[ThisMode]]` is the discriminator: lexical for an arrow and for nothing else in this engine. An async arrow keeps its kind (the evaluation derives `%AsyncFunction.prototype%`, and the record carries it too), and a restored arrow still has no `prototype` — the one own property an arrow must not gain. The limit is the arrow's whole point: a restored arrow's `this` and free names are the reading realm's global, which the module docs state.

*And the console was the bridge's own gap.* With host callbacks carriable, `create_blob` stopped on `console.log` — not a deno callback at all but **the bridge's** console, which `Context::new` installs on every context and no host had any chance to register. The bridge now provides the table entry itself, on both sides, and **first**: a host's table is legitimately a different length at write and at load (deno externalizes its lazy sources only while snapshotting), so an index that depended on that length would resolve against a different entry on each side. No format change and no engine change — `snapshot::engine_table` is a few lines and a comment.

*Tests.* Three engine (a round trip that is callable and still has no `prototype`; a fresh arrow's source; an async arrow's kind) and one bridge (an arrow attached as context data, called after the restore), the re-aimed refusal case, and two mutations. The console has its own bridge test: attached as data, restored, still named `log` and still silent.

Gates: `cargo fmt --all -- --check` clean; `cargo test -p runtime --lib` **840 → 843 passed / 0 failed**; `cargo test -p v8 --features simdutf --lib -- --skip the_data_a_built_function_carries_survives_a_collection` **221 → 223 passed / 0 failed / 1 filtered**; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,214 → 5,219 passed / 0 failed / 4 ignored**. `crates/runtime` changed, so the battery ran and every number is the certified one — including the corpus, which is what would catch an arrow's new `[[SourceText]]` being wrong somewhere: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

*And the next value is a function the **engine** left without a source, for a reason that is a bug rather than a gap.* Re-measured, `create_blob` stops on `async_op_0` — an ordinary function `00_infra.js` defines, created by `setUpAsyncStub` when the **host** calls it from Rust. A probe in `capture_source` printed what the stack looked like at that moment: **two frames, none of them carrying a source**. The host call pushes the callee's execution context with `source: None` (the certified call path, whose own comment claims the slot "is never consulted here" — true for the body, false for a closure created *inside* it), and because the call came from Rust there is no script frame beneath it to fall back on. So `[[SourceText]]` is missing for **every** closure created inside a function a host calls, and the same absent slot is what `source_hash_at` warns about when it degrades a body-cache key to raw addresses. The fix is one slot per call frame — the **callee's** own source, which is by definition the text its spans refer to — and it is the next slice.

**The text a call frame runs — landed, and deno's build stops on a method.** The eleventh part of ledger item 16. It closes the blocker the tenth part's measurement named (`async_op_0`, a closure created inside a function the host calls from Rust) and, while measuring, turned up a second defect of the same family that had been silent.

*The fact the frame needed was not `[[SourceText]]`.* `EcmaFunction.source` is the function's own **slice** of some larger text — what `toString` answers — while a body's AST spans are offsets into the text it was parsed **from**. So the record gained `parse_text`, and the frame a call pushes runs that: `register_function` fills it from the walk `capture_source` already uses (`enclosing_source(body.span)`), a caller that parsed the body from a text it holds passes it (`instantiate_function_from_source` — CreateDynamicFunction and the restore), and the module's declaration pass, which registers under a **bootstrap** context with no text of its own, sets it from the module text it already slices `[[SourceText]]` out of, beside the `declaring_module` it already patched. `instantiate_arrow` computes it for the arrow's span. **Nine push sites** carry it: the certified call and construct, the certified and slow tails, the slow call and construct, and the async, generator and async-generator calls — the certified pair carried `None` and the rest the **caller's** text, which is right only when the two texts happen to be the same one. The certified call's fast-path tuple became a named `FastCall`, since the frame needs one more field than a four-tuple reads well with.

*The defect it exposed was a wrong slice, and it was there before this change.* A closure created inside a `Function`-built body resolved its span against the *script* that called `new Function`, because the resolution's fit check (`end <= len`) passed coincidentally: `nction('return function stub()` — the script's text cut at the assembled text's offsets. The dynamic and restore paths now hand the record the text they parsed, so the walk is not consulted for them at all; the `makeDynamic` case of `a_called_body_runs_its_own_text_not_the_callers` fails on the wrong slice without it. That this was a *pre-existing* wrong answer rather than a missing one is why it stayed invisible: a wrong `[[SourceText]]` is a `toString` that returns text, and only a span past the end of the wrong text refuses.

*What it makes visible.* `is_frame` counts a context with a source as a frame, and that criterion was written when a source meant a script, a module or eval — the certified push's context is the only frame an ordinary call has, so a trace now reports one frame per call rather than only the calls that read a spec-only slot. Two consequences are stated rather than hidden: those frames carry **no name** (the certified push keeps `context.function` gated on its one reader, a sloppy mapped `arguments`, so `GetFunctionName` answers `None` for them), and `Error.stack`, which names its lines from that same slot, reads exactly as it did. A **leaf-inlined** call still pushes nothing, by design and soundly — leafhood excludes closure creation, calls and a sloppy `arguments` — and it is not observable from JavaScript at all: reaching a trace means calling, and a call disqualifies leafhood.

*Tests.* Four in `crates/runtime/src/api/mod.rs` (the callee's text over six call shapes — certified, arrow, env-path, `Function`-built, and both tails; the three suspended bodies; the construct path; a **module** function the host calls with no module frame beneath it) and two in `snapshot.rs` (the host-call shape round-tripping through the format, which is what deno's walk refused; a class defined in a `Function`-built body, which the walk used to refuse and now carries). Two existing tests were **re-aimed rather than deleted**, because part 11 removed the constructions they used: the class-source refusal (its `new Function` shape is carried now) became that positive test, and the arrow case of the refusal-by-kind test became the **accessor** — the kind that still had no `[[SourceText]]` at all, and then the only source-less refusal the engine's tests reached (part 17 gave the accessor its source too, so that refusal is positive coverage now and no function kind is source-less). The bridge's `a_capture_reports_the_engines_contexts_not_the_call_stack` pins the new frame list. **Twelve mutations, each caught**: the certified call push, the certified construct push, the slow call push, the three suspended pushes, the certified tail push, the slow tail push, the arrow's `parse_text`, the registration walk, the dynamic path's explicit text, and the module declaration pass's.

Gates: `cargo fmt --all -- --check` clean; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test -p runtime --lib` **843 → 848 passed / 0 failed**; `cargo test -p v8 --features simdutf --lib -- --skip the_data_a_built_function_carries_survives_a_collection` **223 passed / 0 failed / 1 filtered**; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,219 → 5,224 passed / 0 failed / 4 ignored** (one earlier run of it reported a single failure whose name was not captured; the two runs after it, one of them the counted one, were clean). `crates/runtime` changed, so the battery ran, twice — once mid-work and once against the tree as it stands, with the release binaries rebuilt after the last engine edit (a file mtime newer than `target/release/sweep.exe` was caught and is why the second run exists): test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

*And the next value is a method.* Re-measured the same way, deno's `create_blob` now gets past `async_op_0` and stops on `v8::SnapshotCreator::create_blob: the engine cannot carry a method (a method's source has no `function` keyword, so it cannot be re-parsed on its own) yet` — ledger part 8, the kind the seventh and tenth parts both left named as open. A method's `[[SourceText]]` is method-form (`m() {}`) and needs a parse context of its own plus its carried `[[HomeObject]]`; the same context is what an accessor needs (and got, in part 17, by the same `({ … })` wrapper).

**A method — landed, and deno's walk reaches the realm's own builtins.** The eighth part of ledger item 16, designed and implemented in one step because the measurement named it exactly.

*What the value is, measured.* A probe in the walk's method refusal printed `name=None method=true ctor=false async=false gen=false strict=true lexical=false source=Some("__setTickInfo(buf) {\n      tickInfo = buf;\n    }") home=Some("object")` — an ordinary, strict object-literal method with an object home object and no `name` on the record (its `name` is an own property, which the property tail already carries). So the engine has kept the `[[SourceText]]` all along — a method's `toString` answers `m() {}` — and what the record lacked was the *grammar* to read it back by.

*The grammar, and the value it carries with it.* `GRAMMAR_METHOD` (byte 3) and, because a method is the one callable whose `[[HomeObject]]` is part of what makes it one, a home-object serial in `REC_FUNCTION` written and read only when the grammar says so — the layout stays one reader and one writer with a grammar-conditional field. The walk's method refusal became `Grammar::Method` carrying `data.home_object`, and `visit` descends into the home object like every other value a carryable's state is made of, so a home the format cannot carry refuses by name as any other edge does. `build_evaluated_function` gained the method case: it evaluates `({<source>})` — the object literal a MethodDefinition is only valid inside — and takes the temporary object's **first own property descriptor**, which is what lets one path serve all three forms (a data property's value is the method, an accessor's `[[Get]]`/`[[Set]]` is the getter/setter), then `make_method` puts the record's home object back over the temporary one. The restored method's own source is re-captured by that evaluation, so it answers `m() {}` and a second round trip is stable.

*The strict wrapper, which the evaluated grammars had been losing.* A class method is strict whatever its body says and an object method inherits the strictness of the code around it, so the record's `strict` byte decides whether the wrapper is strict *code* (`"use strict"; ({<source>})`). The span arithmetic is unaffected — the prefix shifts the text the method's own slice is taken from, and the slice is still the method — and the same one line closes the **arrow** record's version of the gap, which had been restoring a strict arrow sloppy since the tenth part.

*And the parse entry had been lying about why.* Writing the mutation for the grammar choice is what showed it: with a method written through `Grammar::Function`, the method's `round_trip` *passed*. `parse_function_expression` consumes the `function` keyword without checking it, so `parse_function("m() { return 41; }")` reads the `m` as if it were the keyword and answers an *anonymous* function with the same body — which is why the grammar's own refusal message ("it cannot be re-parsed on its own") was not what happened. `parse_function_with_async` now checks for the keyword (the parser's own test pins it), which is the premise the grammar choice rests on, and reading a method as a function is now refused where it used to be silently wrong: it loses `[[HomeObject]]` — a body with `super` fails as an early error, `super is only valid inside class methods` — and gives the closure the deferred `prototype` a plain function gets, which a method must never have.

*Tests.* Four engine (a method round trips, is callable, answers its source, and has **no** `prototype`; a method reads `super` through a home object set by `__proto__`, which is the test the serial exists for; a class **with** a method round trips, constructs, and its method is still strict — the shape deno reaches next — while an object method written in sloppy code comes back sloppy; and a strict arrow comes back strict while a sloppy one stays sloppy), one bridge (a method attached as context data, restored, reaching `super` from a script — the same restore path the engine mutation guards), and the refusal-by-kind test re-aimed to the **accessor** alone, since a method is positive coverage now. **Eight mutations, each caught**: the method grammar read as the function grammar, the home serial not written, not applied, not walked, read but ignored, the wrapper never strict, the wrapper always strict, and the parse entry's keyword check.

Gates: `cargo fmt --all -- --check` clean; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test -p runtime --lib` **848 → 852 passed / 0 failed**; `cargo test -p parser --lib` 86 passed / 0 failed; `cargo test -p v8 --features simdutf --lib -- --skip the_data_a_built_function_carries_survives_a_collection` **223 → 224 passed / 0 failed / 1 filtered**; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,224 → 5,229 passed / 0 failed / 4 ignored**. `crates/runtime` **and** `crates/parser` changed, so the battery ran: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail. One `all` run before the re-run reported crashes in a `decodeURIComponent` batch and two typed-array hangs and is recorded as **environment**: the disk was at 100% (18G free) during it, the named fixtures pass one by one and whole-batch, and every run since — including the last, with the release binaries rebuilt after the final source edit — reproduces the certified numbers exactly.

*And the next value is the realm's own builtin functions.* Re-measured the same way, `create_blob` now carries the method and stops on `a built-in function (a host callback is a Rust closure, not a name or an address a snapshot can carry)` — with the probe saying `name=Some("abs") own_name=Some("abs") length=Some(1.0) construct=false host=true`, i.e. **`Math.abs`**, a value the host's table legitimately cannot hold because the *engine* made it. It is reached as a value rather than through `%Math%` (which the walk stops at, being named) because deno's `00_primordials.js` copies the realm's builtins into its primordials object — `MathAbs`, `MathMax`, `ArrayPrototypePush`, … — so the walk can meet *any* builtin the realm installs. What that wants is part 6's own mechanism applied past the ten globals: a builtin function the reading realm rebuilds should be written by **name**, which means the realm has to name the functions it installs on its intrinsic objects. That is the twelfth part, and it is the next value.

**The realm's own builtins by name — landed, and deno's blob both builds and is read for the first time.** The twelfth part of ledger item 16, and the value the method record's measurement named: `Math.abs`, a builtin the *engine* made, reached as a value because deno's `00_primordials.js` copies the realm's builtins into an object it attaches to the realm it snapshots.

*The mechanism is the sixth part's, extended past the ten globals to what the realm's own objects hold.* `Intrinsics::name_members()` runs last in the bootstrap, after every install has declared its names, and derives a name for every **function** member of every registered intrinsic object and function: `%<owner-stem>.<key>%` for a data property, `%get …%`/`%set …%` for the halves of an accessor, and `%<stem>[Symbol.iterator]%` (no dot) for a well-known symbol key — the spellings the installs that declare a member already use (`%Array.prototype.at%`, `%get Error.prototype.stack%`). The name is derived rather than declared at each site because the derivation is a property of the realm's own structure — the owner's registered name and the member key — which is exactly what the writing and the reading realm share. Measured: **355 names** for a realm this engine boots.

*Three rules the pass keeps.* It reads the name table into a scratch `Vec` before writing anything (`define` takes the `entries` borrow); it leaves a name already in the table alone, so a derivation can never displace a declared name; and it names only **functions**, because naming a random object would be a false intrinsic claim. `name_function` lifts the plain registration out of `define` so a derived name does no dispatch-handler lookups — it cannot need one, because a member's call reaches its handler through the names its *install* declared and the name table is per function.

*And the pass that claimed to be idempotent was not, which a measurement showed.* The doc said "running the pass twice is a no-op"; a probe counting what each call added printed **355, then 120**. The second call had scanned the *first* call's derived names as owners — `%Array.prototype.constructor%` names `%Array%` — and minted second-order spellings (`%Array.prototype.constructor.from%`) beside the declared ones (`%Array.from%`). The pass now remembers the names it derived and skips them as owners, so it is idempotent as documented, and the test asserts it (a second call leaves `%Array.prototype.constructor.from%` absent); removing the skip reproduces exactly that name.

*What it promises.* A named builtin comes back as the **reading realm's own** value — the same function object, not a copy — because the restore resolves the name against the realm it reads into. That is what makes deno's `uncurryThis` shape survive: `Function.prototype.call.bind(Math.max)` has a bind's *target* that is a named builtin.

*Tests.* Two: `realm::tests::a_realm_names_the_members_of_its_own_builtin_objects` (Math.abs named by stem and key; an accessor's getter named with the `get` prefix; a name already present not displaced; and the idempotence assertion above) and `snapshot::tests::a_builtin_member_round_trips_as_the_realms_own` (Math.abs by name, with the blob asserted to carry `%Math.abs%`; a bind over `Math.max` restored and called; `Number.isFinite` on a function owner — a constructor's static, which nothing dispatches through, so no install names it of its own accord). Six mutations, each caught: the pass not run, the stem keeping its percents, accessors not named, function owners not scanned, the owner dropped from the name, and the idempotence skip.

Gates: `cargo fmt --all -- --check` clean; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test -p runtime --lib` **852 → 854 passed / 0 failed** (853 with the documented flake `certified_body_global_read_fast_path_stays_spec_exact` skipped); `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,231 passed / 0 failed / 4 ignored**. `crates/runtime` changed, so the battery ran with the release binaries rebuilt after the last engine edit: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

*And deno's `create_blob` completes.* Re-measured with the build script's output forced (`touch libs/core/examples/snapshot/build.rs` — cargo caches build-script results), `cargo build --offline -p build-your-own-js-snapshot --features deno_core/v8` builds and its build script runs to the end, writing a 226,426-byte `RUNJS_SNAPSHOT.bin`. **This is the first time deno's `create_blob` has completed.**

*And the next value is at the load, which had never been exercised against deno's blob.* `create_snapshot` writes the blob; `JsRuntime::new` with `startup_snapshot: Some(...)` reads it, and that is the example's own `check_output` test. It now reaches the restore and panics: `v8::Context::FromSnapshot: the snapshot could not be read for context 1: snapshot function could not be rebuilt: its method source could not be evaluated: SyntaxError: Private field access is only valid inside a class`. A **method whose body reads a private name** — deno's bootstrap copies such methods out of classes into its primordials — cannot be rebuilt by evaluating it in the object-literal context the method record uses, because a `#name` is only in scope inside a class body. That is part 13, and it is the next value.

**A method that reads a private name — landed, and deno's blob reads past the value it stopped on.** The thirteenth part of ledger item 16, designed from the load's own measurement.

*What the load stopped on, measured.* A probe in the restore's failure path printed the source of the value it could not rebuild: `enter(value) { const previousContextMapping = getAsyncContext(); this.#enterWithPreviousContext(value, previousContextMapping); return previousContextMapping; }` — an ordinary class method calling a **private method**. A `#name` is in scope only inside a class body, and the method record evaluates its source in the object literal a MethodDefinition is only valid inside, so that rebuild cannot work. Rebuilding it by evaluating the class source *again* was rejected too: a private name belongs to one class declaration, so a second evaluation's brand is not the brand the class the graph holds gives its instances, and the method would throw when called on one.

*The trigger is exact.* `EcmaFunction` already records the private environment a closure resolves `#name` through, and `build_class` installs the class's own on every method it instantiates, so the walk asks whether that environment **has names**: a method that could resolve one is carried as its class's member, and a method that could not keeps the record it had. That is what leaves every class with no private elements — and so every existing class test — on the old path.

*Format.* `REC_CLASS_METHOD` (tag 17): the class, the member's key as a slot of its own (a string or a symbol, so a computed or symbol-keyed method is the same record), the form (data, getter, setter — which is what tells an accessor's two members apart when they share a key), the home object, and the function tail. **The source is not carried**: the class the restore evaluates already holds the method.

*The class is identified only when it is certain.* A static method's `[[HomeObject]]` is the class itself; an instance method's is a class constructor's own `prototype`, compared **by identity**, so an object that merely carries a class as its `constructor` is not mistaken for a prototype. The member's key is found by identity — the key whose descriptor holds *this* function — which makes a computed key no harder than any other. A class **constructor** is excluded, and a stack overflow is what showed it: its own `constructor` property names the class as a member of itself, so the record resolved through itself forever.

*Restore.* The class is materialized first, and its build keeps the `prototype` its own evaluation made — stashed by class serial and pinned — before the record's property tail replaces it with the carried prototype. The member is read out of that prototype, or out of the class itself for a static, by key and form, and `make_method` re-homes it to the record's home object so `super` resolves through the rebuilt prototype rather than the evaluation's orphan. One re-entrancy is handled explicitly: the carried prototype's own properties include this very member, so the class's build reaches the record while the record — reached first as a value, as a `bind` target — is being built; the record re-reads the builder's `made` table after materializing the class and returns that build rather than making a second method.

*Tests.* Three: `snapshot::tests::a_method_that_reads_a_private_name_round_trips_as_its_classs_member` (the deno shape — a public method calling a private one — called on an instance of the restored class, so the private brand has to match, with the blob asserted to spell the record), `snapshot::tests::a_private_name_method_reaches_super_through_the_carried_prototype` (the re-homing, whose discrimination is that the mutation is on the carried prototype and only the carried object has it), and one bridge (`a_private_name_method_round_trips_and_reads_its_classs_private_field`, the same through attached context data). Three mutations, each caught: the trigger disabled (the blob carries no class-member record), the re-homing dropped (`super.added` is `undefined`), and the constructor guard removed — which the existing private-field test turns into a stack overflow.

Gates: `cargo fmt --all -- --check` clean; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test -p runtime --lib` **854 → 856 passed / 0 failed** (855 with the documented flake `certified_body_global_read_fast_path_stays_spec_exact` skipped); `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,231 → 5,234 passed / 0 failed / 4 ignored**. `crates/runtime` changed, so the battery ran with the release binaries rebuilt after the last engine edit: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

*And the next value is the class's own evaluation.* Re-measured, deno's blob now reads past the private-name method — the first time that value has been carried — and the restore stops on the **class**: `v8::Context::FromSnapshot: the snapshot could not be read for context 1: snapshot function could not be rebuilt: its class source could not be evaluated: ReferenceError: "SymbolIterator" is not defined`. That is part 7's stated divergence arriving as a hard failure: a class's definition-time code runs again at restore, so a **computed key** (or a static field initializer, or a `static {}` block) that reads a name from the module which defined the class finds the reading realm's global instead. The engine already has the mechanism that removes the re-evaluation — `class_definition_evaluation_with_scope` takes the element keys **precomputed** — so the next part is to carry them, which is part 14.

**A class's definition-time inputs — landed, and deno's blob reads past the class.** The fourteenth part of ledger item 16, and the value the part-13 record's measurement named.

*What the load stopped on, measured.* A probe in the restore's failure path printed the class the class record could not evaluate: `class SafeArrayIterator { #array; ... [SymbolIterator]() { return this; } }` → `ReferenceError: "SymbolIterator" is not defined`. A class is rebuilt by evaluating its source (part 7's design, and it stays: what a class *is* — `[[ConstructorKind]]`, fields, private methods and environment, prototype object, the constructor's body — is what only the engine's own class evaluation makes), and the definition-time code then runs again: part 7 stated that as a divergence. The load shows it is not benign — a **computed key** that reads a name from the module which defined the class cannot be evaluated at all in the reading realm.

*What is carried is the input, not a re-derivation.* `build_class` already resolves every computed public element's key through `element_key_with`, in exactly the order the class evaluation's `precomputed_keys` are indexed by, so the constructor's record now keeps them (`EcmaFunction::computed_keys`). The heritage goes with them: `resolve_heritage` derives the proto parent from the heritage *value*'s `prototype`, so carrying the value reproduces the class exactly — and `extends null`, whose resolved form is `%Function.prototype%` as the super constructor with no proto parent, is carried as the value `null`, which the writer can tell apart because no other heritage resolves there.

*Format.* The class grammar's record gains, in the grammar-conditional slot a method uses for its `home`, a heritage serial (`NO_REF` for a class with no `extends` clause) and a count-prefixed list of key serials; both are ordinary value slots, so a symbol key is the symbol it is.

*Restore.* The source is parsed as `(<class source>)` — the expression form a class source is only valid as — the class expression taken from it, and the evaluation handed to `class_definition_evaluation_with_keys`, the engine's own entry for a class whose heritage and keys were evaluated elsewhere (its resumable-VM path). Nothing is re-evaluated. The parse is a script, so the class's spans index the text the parse was given, and the bootstrap context the evaluation runs under carries that text as its `source` — which is what keeps `capture_source` answering the class's `[[SourceText]]`, so `toString` and a second snapshot are unaffected.

*What it still does not carry:* a `static {}` block and a static field initializer *execute* at restore, because they are effects rather than inputs. That stays part 7's stated divergence.

*Tests.* Four in `crates/runtime/src/snapshot.rs`: a computed field key reading a module binding (plus the restored class's `toString`), the heritage (the implicit constructor calls `super()`, which only the heritage decides — said in the test because the statics would survive without it, carried by the class's own prototype link), `extends null` (the one heritage whose resolved form is not its written form), and a computed *method* key with a private element (both halves at once). Three mutations, each caught: the keys not written (both key tests fail with deno's own error), the heritage always `None` (both heritage tests fail — the class comes back a base class), and the evaluation's frame left without a source (the native form instead of the class source).

Gates: `cargo fmt --all -- --check` clean; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test -p runtime --lib` **856 → 860 passed / 0 failed** (859 with the documented flake `certified_body_global_read_fast_path_stays_spec_exact` skipped); `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,234 → 5,238 passed / 0 failed / 4 ignored**. `crates/runtime` changed, so the battery ran with the release binaries rebuilt after the last engine edit: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

*And the next value is an object-literal method with a computed key.* Re-measured, deno's blob reads past the class — the first time the class's own evaluation has succeeded — and the restore stops on `its method source could not be evaluated: ReferenceError: "SymbolIterator" is not defined`, whose probe says `home_owner=None`: the failing value's home object is a plain object, so it is an **object-literal method** with a computed key (`{ [SymbolIterator]() { return this; } }`), not a class member. Neither mechanism reaches it — there is no class to take it out of, and the method record's own source is what evaluates the key. It wants the third route: parse the MethodDefinition and **instantiate** the method rather than evaluate the source, so the key expression is never evaluated at all. That is part 15.

**A method's source is instantiated rather than evaluated — landed, and deno's blob now reads through every value the walk carries.** The fifteenth part of ledger item 16, and the value the part-14 record's measurement named.

*What the load stopped on, measured.* A probe in the restore's failure path printed the source of the value it could not rebuild: `[SymbolIterator]() { return this; }` — an **object-literal** method with a computed key, and the probe also printed its home object's owner: `home_owner=None`, so the home is a plain object. That is what rules out the two mechanisms already in the tree: there is no class to read the member out of (so no class-member record), and the home object's own property at that key *is* this record (so it cannot be read out of the object either). The method record rebuilt by evaluating `({<source>})`, and that evaluation evaluates the key expression, which the reading realm cannot resolve.

*The key is not needed to instantiate a method.* A key is only what *installs* a method on an object; the record wants the method alone, and its property tail already carries the method's own `name`. So the rebuild parses the MethodDefinition and instantiates it from the AST: `instantiate_method` for `key(...) {}`, `instantiate_accessor` for `get`/`set` — the same two calls `build_class` makes for a class's elements. Nothing evaluates the key, so nothing needs the name it reads. The parse is `({<source>})` as a script, its one property taken from the AST rather than from an evaluated object; the bootstrap context carries that parsed text as its `source`, which is what `capture_source` resolves the method's `[[SourceText]]` from; and strictness is the record's byte handed to the instantiation rather than a `"use strict";` prefix, which keeps the spans exactly the parsed text's. A class keeps its own evaluation and an arrow keeps its own, so `build_evaluated_function` now serves the arrow alone and `method_of` is gone with the evaluated form.

*And the defect it uncovered, fixed with it.* `define_properties` materialized a property's `get`/`set` serial unconditionally — but an accessor's **absent** half is the `NO_REF` sentinel, so a getter-only property sent it to `materialize`, which answered `Truncated` (`snapshot ends in the middle of a field`). **No test had ever carried an object with an accessor property**, which is why the reader kept it; deno's blob was the first graph to reach one, and only once the object-literal method above materialized. The reader now keeps an absent half absent, which is also what a re-define needs to leave it `undefined`.

*Tests.* Two in `crates/runtime/src/snapshot.rs`: `a_method_with_a_computed_key_round_trips_without_evaluating_it` (called through the key the object holds it under, and still answering its own source) and `an_accessors_absent_half_stays_absent` (a getter-only and a setter-only property, whose present halves must be the reading realm's own — asserted on the *descriptor*'s identity, since reading a property whose getter is `Math.abs` calls it). Three mutations, each caught: the wrapper evaluated (the test fails with deno's own `SymbolIterator is not defined`), the evaluation's frame left without a source (the restored method answers the native form), and `make_method` dropped (the method part's `super` test fails); restoring the old accessor code fails the accessor test with the original `Truncated`.

Gates: `cargo fmt --all -- --check` clean; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test -p runtime --lib` **860 → 862 passed / 0 failed** (861 with the documented flake `certified_body_global_read_fast_path_stays_spec_exact` skipped); `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,238 → 5,240 passed / 0 failed / 4 ignored**. `crates/runtime` changed, so the battery ran with the release binaries rebuilt after the last engine edit: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail. One `all` run reported 34 crashes and 2 hangs and is recorded as **environment**, the same artifact the earlier records name: it was the run whose release build had just finished, and the immediate re-run reproduces the certified numbers exactly (0 fail / 0 crash / 0 hang).

*And the next value is not the engine's.* Re-measured, deno's blob now reads through every value the walk carries — the engine's restore succeeds — and the load stops inside **deno_core's own extraction**: `load_snapshotted_data_from_snapshot` reads indices `0..data_count` (`data_count = 5`) and index 2 answers `None`, so it panics with `NoData { expected: "v8::data::Data" }`. The blob's slot holds **two** items. Measured on the write side: deno attached five, of which three are `Payload::Module` — a module record, which is not a language value — and the bridge's `items_of` maps through `engine_value`, which turns a `Data` into a `Value` or nothing, so it **dropped** them silently and the host's index numbering shifted instead of failing. That is a bridge defect with a design decision behind it (carry a module record by specifier, or refuse the attachment loudly), and it is part 16.

**A module record is an item a slot carries — landed, and deno's blob now yields every item it attached.** The sixteenth part of ledger item 16, and the value the record above measured. It is the first part of this item whose defect is the **bridge's**: on the write side deno attached five items to its bootstrapped context — three of them `Payload::Module`, the ES module records its module map serializes, plus the promise-rejection callback and the `import.meta` prototype — and `items_of` mapped every `Global<Data>` through `engine_value`, which turns a `Data` into a `Value` or nothing, so a module became `None` and `filter_map` **dropped** it. The host's own loader reads its items back by index, so the blob did not merely lose a value: it handed index 2 the item that belonged to index 4, and the load panicked in deno's extraction rather than at the engine's boundary.

*The decision is to carry the record, and the shape follows from what a module record keeps.* Refusing the attachment loudly was the alternative and it is the wrong one: deno attaches module records **on purpose** — its module map is one of the things it snapshots — so a refusal would cost the host its whole snapshot to report a value the engine can in fact rebuild. A `SnapshotItem::Module` therefore carries the two things the engine's own compile path needs, the module's **name** and its **source**, and those are what a record has: a `SourceTextModule` keeps the name it was compiled under and the text it was compiled from, and the **specifier is the host's**, not the record's (`Module::register` is what writes one into the realm's loaded-modules table, and a host re-registers a restored module under its own specifier, which is what deno's `update_with_snapshotted_data` does). What is not carried is not claimed: the restored module is fresh and unlinked, so its link status, bindings and namespace are the ones a newly parsed record has.

*Format and engine.* `REC_MODULE` (tag 18) is a presence byte and the name's units, then the source's units. `SnapshotItem` is now `Value | Module`, so a slot's `items` is an **item** list rather than a value list — and the two paths that can only answer with a value refuse a module rather than guessing: `decode`, whose single-value form is slot 0's index 0, and `Builder::materialize`, which a record *field* uses, because only a slot's own item list can name a module. The restore compiles through `crate::module::parse_module` under a bootstrap context it pushes and pops on every path: the engine compiles a module in the agent's **current realm**, and the realm a slot's items belong to is the one being rebuilt. `remember` pins the module with `pin_handle` like any other record, which is what keeps it across a collection — it is not a language value, so no handle scope holds one.

*The item names `api::Module`, and that is what keeps the bridge out of the engine's internals.* `SnapshotItem::Module` carries the engine's own handle type rather than a raw `Handle<SourceTextModule>`, so the bridge neither reads nor builds one: it hands over the `api::Module` its `Payload::Module` already holds and takes back the one the engine returns. The only engine addition is one `pub(crate) fn handle` on `api::Module`, for the identity the format keys a module by (`from_handle` was already there), and nothing new became public.

*And the bridge stops dropping anything, at the point where the host can act.* `items_of`'s `filter_map` is gone, and the item is decided in `add_context_data` — the attach itself, which is where V8's own check lives and where a host has a frame it can act on. Anything that is neither a value nor a module is refused **by kind** and by position: a new `Payload::kind` answers "a context", "a script", "a stack frame" (a `Context` is a `Data` the crate's own tag checks allow, so this path is reachable), and the panic names the slot and the index. The attach stores an `Attached { item, held }` pair — the item read once, the handle kept for its pin — which is also what makes `items_of` total without a second refusal path.

*Tests.* One in the engine: `a_module_is_an_item_like_any_other` — a module at index 0 and a number at index 1 of one slot, the module read back as a module with the source it was compiled from (asserted on `source_text`) and the number still at index 1, which is the index claim the defect broke. Two in the bridge: `an_attached_module_round_trips_at_its_own_index` — deno's shape, a module and then a value, each read back once through `get_context_data_from_snapshot_once` — and `a_context_attached_as_context_data_is_refused`, the refusal by kind. Four mutations, each caught: `materialize_item` refusing a module (the engine test fails with `Truncated`), answering `undefined` instead (it fails on "the module came back as a language value"), `items_of` filtering modules out again (the bridge test fails **exactly as deno's load did** — index 0 answers the number and the cast to `Module` refuses), and `Payload::kind`'s context arm answering "a value" (the refusal no longer names the kind).

Gates: `cargo fmt --all -- --check` clean; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test -p runtime --lib` **863 passed / 0 failed**; `cargo test -p v8 --features simdutf --lib -- --skip the_data_a_built_function_carries_survives_a_collection` **227 passed / 0 failed / 1 filtered**; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,240 → 5,243 passed / 0 failed / 4 ignored**, the three being this part's three tests. `crates/runtime` changed, so the battery ran with the release binaries rebuilt after the last engine edit: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

*And the next thing is not the engine's, and not a value either.* Re-measured the same way, deno_core's own extraction now runs to the end — all five indices read — and the load dies one step later, in `initialize_deno_core_ops_bindings` (`libs/core/runtime/jsruntime.rs:1087`): deno's `bindings::get::<v8::Local<v8::Object>>` panics "unable to convert" because `globalThis.Deno`, the first of the three objects it reads, is not there. That is the framework's own assumption rather than a missing record. On `InitMode::FromSnapshot` deno **skips its bootstrap because V8 deserializes the heap**: `jsruntime.rs:1080-1083` skip `initialize_deno_core_namespace` and `initialize_primordials_and_infra`, and `1264-1279` skip the virtual ops module and the builtin sources, so `Deno`, `Deno.core` and `Deno.core.ops` can only come from the image — and `store_js_callbacks` reads `Deno` unconditionally at `1282` as well. Deno's five attached items are not that state (the module map, the templates, two handles), so no further item carries it. What this engine *can* serve is the **from-source** path, and this run already shows it: the example's build phase (`create_snapshot` with `startup_snapshot: None`, so `InitMode::New`) ran the whole bootstrap on this engine — ops bound, builtin sources evaluated, the extension's `esm_entry_point` module instantiated — and wrote a 225,661-byte blob. So the next part is a decision rather than a record, and it is §12's eleventh item.

*And that decision's first candidate was measured rather than argued.* Asked to explore **carrying the realm's global object**, the probe described below was run inside deno's own snapshot build — with the build's external-reference table and host, so the numbers are the real walk's conditions rather than a test's — and it **completed on both contexts**: slot 0, deno's empty default context, is 244 records / 6,912 bytes, and slot 1, the bootstrapped realm (`Deno`, `Deno.core`, the ops object, `callConsole`), is **2,833 records / 220,054 bytes, `Ok`**. Nothing reachable from a bootstrapped deno realm's global is one of the kinds the walk refuses — no proxy, no typed array, no host object — so the *write* half of that option is available today and costs about what the blob already costs (225,661 bytes for deno's five attached items). What that measurement could not settle is the **read** half, and the record below settles it — including the defect it had to fix on the way, which is a `runtime` edit made inside a measurement rather than named before it (§11's working rules ask for the other order; it is stated here rather than smoothed over).

**Carrying a realm's global — measured end to end on deno, and the reader had been refusing a frozen object's properties.** The experiment §12's eleventh item called for: walk a bootstrapped global, write it, restore it into a **fresh isolate**, apply it to that realm's global, and call a restored bootstrap function. Probes in, numbers out, probes out again.

*The write half.* With the build's own external-reference table and host, each context's `globalThis` walks to the end — slot 0, deno's empty default context, 244 records / 6,912 bytes; slot 1, the bootstrapped realm (`Deno`, `Deno.core`, the ops object, `callConsole`), **2,833 records / 220,054 bytes, `Ok`**. Nothing reachable from that global is a kind the walk refuses, and it costs about what the blob already costs (225,661 bytes for deno's five attached items).

*The read half found the defect.* The first round trip restored `Deno` with a `Deno.core` of **0 own keys** where the writer's had **147** — while `Deno.core` and `__bootstrap.core` still aliased each other, so it was one object losing its properties, not a numbering slip. Measured in the format: the writer wrote all 147 (and 832 for `Deno.core.ops`) with `extensible=false` — deno **freezes** those objects — and the reader *refused* all 147 (`refused 147 of 147`), because it applied the record's `extensible` flag **before** defining the record's properties. `[[DefineOwnProperty]]` refuses to add a property to an object that is already non-extensible, `define_property_key` answers `Ok(false)`, and `define_properties` discarded that answer — `map_err(...)?` sees only `Err`. Fixed at **all six** reader arms that set the flag (`Record::Object`, `Record::Array`, and the four function-shaped ones), each with the reason in a comment. A function's `length` and `name` already exist on a fresh function, so only a property the rebuild has to *add* tells the order apart — which is what the test asserts.

*And with that fixed, the read half works.* The graph restores into a fresh isolate with the writer's `Deno.core` (147 keys, the same names), the aliasing intact, and 72 of 72 properties applied to the realm's global. A restored *bootstrap* function then **runs**: `Deno.core.setUpAsyncStub('probe', () => {}, undefined)` answers deno's own `Error: Too many arguments for async op codegen (length of probe was -1)` — a complaint about the probe's dummy callback, not a `ReferenceError`. The inference §12's eleventh item recorded (that a restored bootstrap function would resolve its module bindings as globals and fail) is therefore **wrong for this graph**, and the measurement is what says so: deno's bootstrap reads its state through `globalThis.__bootstrap`, which a carried global contains. What is still *not* done is the integration — the load path applying a carried global before deno reads `Deno` — which is the next part rather than a measurement.

*Two more things the experiment turned up, neither fixed here.* `Object.assign(function () {}, { tag: 7 })` throws `TypeError: assign target is not an object`: `object_assign` (`builtins/object.rs:1506-1523`) matches `ValueKind::Object` for the target, while `to_object` answers `Value::Function` for a function — and the same match **skips a function source** entirely, so its enumerable own properties are never copied. A function is an object in both roles, so this is a conformance divergence with no fixture over it; it is named in §12 rather than fixed inside a measurement. And an object literal's accessor, created in an eval'd script's argument position and then frozen, refused to encode (`a function — the engine kept no source text for it`); whether `Object.freeze` or the frame it was created in is responsible was **not isolated**, and the regression test carries data properties instead.

*Tests.* One engine test, `snapshot::tests::a_non_extensible_values_properties_come_back`: a frozen object, a frozen array with an own extra property, and a function with an own property a fresh function does not have, each made non-extensible — every one restores non-extensible *with* its properties. Three mutations, each caught: the flag applied before the properties in the object arm ("its properties came back, not just the shell"), in the array arm, and in the plain-function arm ("the own property tag"). The test's first draft was **not** discriminating — it asserted a function's `name`, which a fresh function already has, so it passed under the mutation; it was rebuilt to assert a property the rebuild must add, which is the second time this plan's "a test that cannot fail" rule has been caught by mutating rather than by inspection.

Gates: `cargo fmt --all -- --check` clean; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test -p runtime --lib` **864 passed / 0 failed**; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,243 → 5,244 passed / 0 failed**, the one being this change's test. `crates/runtime` changed, so the battery ran with the release binaries rebuilt after the last engine edit: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

The probes, all removed once they had answered: a `probe_walk` beside `visit` (the walk's `Result` plus the record count it reached); a loop in `api::Context::write_snapshot` that encoded each context's global alone and reported the size; a `probe_realm` in the same file that dumped a realm's `globalThis`/`Deno`/`Deno.core`/`__bootstrap`/`__bootstrap.core` own keys, their `ObjectKind` and their intrinsic names; an `encode_slots` + `read_snapshot` round trip into a fresh `api::Isolate` whose realm then read `__slag_carried`, applied every property and called `Deno.core.setUpAsyncStub`; and four counters inside the format itself (`visit`, `write_properties`, `write_record`, `define_properties`).

**Every function kind the format carries a source for, and `Object.assign` treats a function as an object — landed.** The seventeenth part of ledger item 16, and the two side findings the carried-global experiment above turned up: §12's items 12 and 13 named each fix before the edit (§11's order), and this record closes them.

*The accessor was the last source-less kind, and closing it is a conformance fix as well as a format one.* `instantiate_accessor` took `params` and `body` and registered the function with `source: None`, while `instantiate_method` had captured `capture_source(agent, f.span)` since the method part — so a getter or setter's `Function.prototype.toString` answered the native form where spec 15.4.3 gives it the MethodDefinition's own text, and the snapshot walk refused it with "the engine kept no source text for it". The instantiation now takes a sixth parameter, `source: Option<JsString>`, and the **parser** records what the callers pass: `ObjectProperty::{Get,Set}` and `ClassElement::{Get,Set}` carry a `span` for the whole `get key() { … }`, captured from the `get`/`set` keyword before it is consumed. Every instantiation site hands it over — `class.rs` for a class's two accessor forms, `expr.rs` for the tree-walker's object literal, and `ir.rs` for the compiled path, where the span rides on `Step::ObjectAccessorName`/`ObjectAccessorComputed` (the JIT passes the step **index** to its helper, so `crates/jit` needed no change). The snapshot's rebuild passes the record's **own** text rather than resolving a span, because the `({ … })` wrapper it parses is not the text the accessor's span indexes — the class-constructor shape, for the same reason.

*A consequence for the refusal list: no function kind is source-less now.* The format's own module docs listed "a method or an accessor" as kinds it refused for want of source; both carry theirs (parts 15 and 17), and what remains is a class constructor defined where no frame holds the text its span belongs to — the `new Function` case the class part already re-aimed. So the engine's source-less refusal test (`an_uncarried_function_is_refused_by_kind`) had nothing left to refuse by kind and was **replaced** by positive coverage: `an_accessor_round_trips_with_its_source` round-trips an object with a getter **and** a setter (the setter writes, the getter reads the same restored property back), asserts each half answers its own definition, and reads a class accessor through `new restored().c`.

*`Object.assign` and a function, both roles.* `object_assign` matched `ValueKind::Object` for the target and for each source while `to_object` answers `Value::Function` for a function, so a function **target** threw "assign target is not an object" and a function **source** was skipped entirely — a conformance divergence with no fixture over it. Both ends go through the engine's coercion (`as_object`) now, and the source's properties are read with the coerced source as the receiver, so a getter sees the function rather than its object part.

*The walker arm is parity, and that is measured rather than assumed.* `expr.rs`'s `eval_object_literal` captures the span too, but it is the **tree-walker**'s arm, and the walker is dead in normal execution. Probes in `evaluate_body` and in `eval_statement_list` (the walker's two entries — `ordinary_call`'s `None` arm and `ordinary_construct`'s) fire **0** times across the whole `cargo test -p runtime --lib` suite, while a control probe in `eval_program` fires 2,967 — so `register_function`'s "every body compiles to the step IR" is literally true, and that arm is uncovered by any test and cannot be made discriminating without forcing the walker. It is kept as parity (if the walker ever runs an object literal, its accessor keeps its source) and recorded as uncovered rather than presented as covered.

*Tests.* `builtins::function::tests::function_to_string_renders_source_or_native` gains a getter, a setter, a class getter, and a getter inside a `new Function` body — the last reaches the compiled path through `instantiate_dynamic_function` rather than the walker, which is what the probes settled. `snapshot::tests::an_accessor_round_trips_with_its_source` replaces the refusal test, as above. `builtins::object::tests::assign_treats_a_function_as_an_object` covers both roles and the enumerable-only rule (a function source's non-enumerable `name` does not travel).

*Six mutations, each caught.* The accessor's `source` argument dropped in `instantiate_accessor` (both tests fail); the compiled path's capture dropped in `ir.rs`'s `object_accessor` (both fail — the object-literal assertions are its guard, including the `new Function` one); the class path's capture dropped in `class.rs`'s getter (both fail on the class assertion); the snapshot rebuild's `Some(text)` replaced by `None` in the getter and the setter arm (only `an_accessor_round_trips_with_its_source` fails — the discrimination the replaced refusal test never had); and `object_assign`'s **target** then **source** coercion reverted to the `ValueKind::Object` match (the first throws "assign target is not an object", the second drops a function source's property).

Gates: `cargo fmt --all -- --check` clean; `cargo clippy --locked --workspace --all-targets -- -D warnings` clean; `cargo test -p runtime --lib` **865 passed / 0 failed**; `cargo test -p parser --lib` 86 / 0 and `cargo test -p test262` 3,324 / 0 (2 ignored) plus 3 / 0; `cargo test --locked --workspace -- --skip the_data_a_built_function_carries_survives_a_collection` **5,244 → 5,245 passed / 0 failed** (the one net-new test; the replaced refusal test nets zero). `crates/{runtime,parser,syntax}` changed, so the battery ran with the release binaries rebuilt: test262 `all` 48,622 — **48,464 pass, 0 fail / 0 crash / 0 hang**, 158 skip; `intl402` 3,357 — 3,205 pass, 152 skip; the eight wasm core suites 64,594 checks / 0 fail / 0 pending; the JS-API sweep 1,001 tests / 0 fail.

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
- **The console V8 installs is here, without its tag and without the rest of the
  extras binding object's properties.** `GetExtrasBindingObject` carries
  `console` — the one property `deno_core` reads — and V8's object also carries
  `isTraceCategoryEnabled`, `trace`, and (when the build enables them)
  `getContinuationPreservedEmbedderData`/`setContinuationPreservedEmbedderData`.
  None of those is here, because nothing in the frontier reads them and an inert
  function that looks like a working feature is worse than an absent one. The
  console itself carries V8's method set but not its `Symbol.toStringTag`, for the
  reason the `get_constructor_name` bullet records: a symbol-keyed define means
  reaching past the engine's string-keyed object API. And V8 builds those methods
  without a `.prototype` (`CreateFunctionForBuiltinWithoutPrototype`) where every
  function this bridge makes has one — a shape the crate's own
  `Function::builder` shares, so it is not this console's alone. What a host can
  act on is the same in all three cases: the methods are callable, they are
  enumerable under their V8 names, and they do nothing.
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
- **`get_constructor_name` is the map's answer only in the cases where the
  property chain agrees with it, and the difference is stated where a host would
  read it.** V8 reads the constructor the object's *map* was made with and the
  `Symbol.toStringTag` on its chain; this walk reads the nearest own
  `constructor` as it is now, and cannot read the tag at all — the property reads
  the bridge can make are name-keyed. The consequence is small and pinned by a
  test: an iterator answers "Iterator" rather than "Map Iterator", and a script
  that reassigns `Foo.prototype.constructor` moves the answer here where V8's
  map would hold the original. Landing the tag half means a symbol-keyed read on
  the engine's template/object API, which is a named follow-up rather than a
  guess.
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
- **A template materializes one function per realm, and that is load-bearing now.**
  The crate's `GetFunction` hands back the same function for a (template, context)
  pair; this engine used to make a fresh one — and a fresh `.prototype` — per call,
  which was invisible until `Inherit` arrived: a child's chain wired to a second
  copy of the parent's prototype fails `instanceof` while looking right. The memo
  costs one pinned entry per realm per template (the realm, and the function, which
  reaches the `.prototype` object through its own property), and it settles what a
  property set *after* materialization does: nothing — which is the crate's
  contract, whose documentation requires configuring a template before
  `GetFunction`.
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
- **The engine's templates are string-keyed, and where that shows is stated
  rather than hidden.** `ObjectTemplate::set` and `set_with_attr` panic on a
  symbol key: the crate's `Template::Set` takes a `Name` and this engine's
  template properties are `JsString`-named, so the alternatives were a silent
  no-op or a panic. The attributes half of the cluster has since landed — an
  engine `PropertyAttributes` that a template property carries into the descriptor
  of every instance it makes — and what is still absent is the half that is not
  about *instances*: `FunctionTemplate::{set, inherit}` and a static accessor
  property need the engine to install a property on the function itself and to
  relate two templates, which is the next slice.
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
- **A module request is a payload here, not a heap object.** V8 has a
  `v8::ModuleRequest` inside a `FixedArray` of them; the engine keeps a module's
  requests in the module's own record, so a handle names the record and where in
  it (`Payload::ModuleRequest`) and the list names the record
  (`Payload::ModuleRequests`). Nothing a host can observe differs — the reads are
  the same five — but two consequences are worth stating: a cast *to*
  `ModuleRequest` is refused for anything the bridge did not build (a host's array
  of the right shape is not a request), and a request handle keeps its module
  alive only through whatever keeps the module alive, because a pin is per
  object and a request is not one.
- **A message's position is the bridge's record, and only a failed compile makes
  one.** V8 reads a start position, an end position and the script back off the
  error object itself (`ComputeLocationFromException`, `isolate.cc:3640`); Slag's
  error objects carry no such properties, so `crates/v8/position.rs` records
  `{name, line, column}` per error identity on the isolate, at the one moment the
  bridge can trust the span it has. *Trust* is the load-bearing word: a `JsError`
  span indexes whichever source the failing code was parsed from and does not say
  which source that was, so a runtime error's span may belong to another script —
  those record nothing, and `get_line_number` answers `None`, which is the crate's
  own absent-location channel (its two callers `?` on it, `error.rs:841`). A host
  that needs a runtime position needs the engine's per-activation source
  positions, which is the structured-frames item §10 lists as the next engine
  work. Two smaller divergences ride along: `Message::get` reads an error's `name`
  and `message` with a [[Get]] where V8 reads data properties only, so a getter a
  host installed on either can run while an error is being formatted; and V8's
  `#<CtorName>` form for an object whose `toString` is the default is not
  reproduced, so a non-error object is described by its built-in tag.
  `Message::get_stack_trace` answers `None`, which is V8's default too — nothing
  here captures a trace for a message, and
  `set_capture_stack_trace_for_uncaught_exceptions` is accepted and carried
  nowhere.
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
- **A synthetic module's steps are a function pointer, and the host's function is
  reached through its type.** The crate's `SyntheticModuleEvaluationSteps<'s>`
  names the scope's lifetime in its arguments, and the engine keeps the callback
  in the record for the record's whole life, so the mapped pointer cannot be
  stored — a `.map_fn_to()` value would have to outlive the scope it was mapped
  under. The bridge therefore reconstructs the host's function item and bounds
  `create_synthetic_module` by the higher-ranked `Fn` rather than by the crate's
  `impl MapFnTo<..>`, which is the same shape `FunctionCallback` uses and accepts
  every host function the crate's bound accepts. Two consequences a host can
  see: a failing evaluation answers the rejection the engine made from the
  steps' failure where the crate answers the empty handle (the crate's steps
  report a throw that way, and V8 records `isolate->exception()` as the module's
  error), and `create_synthetic_module` aborts on an engine refusal, which that
  shape has no channel to report.
10. **The wasm tail — both slices landed, and this item is closed.**
  Seven of the then-remaining 19 errors were this one subsystem, and they were two
  capabilities rather than one; both are in §7 with their findings.

  - **The compiled module (4 sites: `libs/core/modules/map/wasm.rs:48`,
    `libs/core/ops_builtin_v8.rs:704` and `:799`, and
    `libs/core/runtime/jsruntime.rs:507`).** `WasmModuleObject::compile(scope,
    bytes)`, `module.get_compiled_module()`, `WasmModuleObject::from_compiled_module`
    and the `CompiledWasmModule` name (which upstream marks `Send + Sync`). The
    engine already does the work — a `WebAssembly.Module` object is an ordinary
    object whose decoded `wasm::Module` sits in `agent.wasm_modules` keyed by the
    object's id (`crates/runtime/src/builtins/wasm.rs:808-836`) — so this is an
    *exposure*, not a capability. **Engine change, named:** `runtime::api` gains
    `api::WasmModuleObject` (`compile`, `from_compiled_module`,
    `get_compiled_module`) over a new `api::CompiledWasmModule` carrying the
    decoded module, plus one `pub(crate)` entry in `builtins::wasm` for the
    existing `compile_module_bytes` path — because what a host's `compile` must
    answer is the *Module object* (prototype and all), not a bare record.
    Existing behaviour is untouched: the api module is additive.
  - **Streaming (3 sites: `ops_builtin.rs:263`, `ops_builtin_v8.rs:1414`,
    `runtime/setup.rs:302`) — landed.** It needed the engine to grow the hook, as
    the survey predicted, and it needed two things the survey did not name: the
    engine's `HostHooks` pair answering *whether* a host streams at all (so an
    isolate without one refuses the call instead of answering a promise nothing
    settles), and an `install_hooks` that keeps the two isolate callbacks from
    displacing each other — the promise-rejection callback had been stored in the
    bridge's hook struct, so the streaming install would have dropped it. The
    upstream alias the plan expected (`IsolateWasmStreamingCallback`) does not
    exist in v150.4.0. See §7 for the rest. The survey behind the slice, and what
    it was right about:

    `Isolate::set_wasm_streaming_callback`, the `WasmStreaming<false>` handle, and
    `WebAssembly.compileStreaming`. V8's shape is fixed
    (`v8/src/wasm/wasm-js.cc:889-911`): `compileStreaming` makes the resolver,
    then `Promise.resolve(source).then(compile_callback, reject_callback)`, and
    the embedder's callback is handed the resolved response together with a
    `WasmStreaming` it feeds and `finish()`es; V8 compiles into it as bytes
    arrive and `DCHECK_NOT_NULL`s that a callback was installed. This engine had
    no `compileStreaming` at all (its `WebAssembly` namespace defined `validate`
    and `compile`), so what was missing was the *host-driven* path, not a decoder
    — as built: an `api::WasmStreaming` record (accumulated bytes, a promise
    capability, the url, a settled state) with
    `on_bytes_received`/`set_url`/`finish`/`abort`, and
    `WebAssembly.compileStreaming` in the engine's JS-API, routing through the
    hook when one is installed and otherwise throwing a `TypeError` (V8
    `DCHECK`s there — the divergence §9 states rather than pretends about).
    Decoding happens at `finish`, not as bytes arrive: the same module and the
    same promise, without V8's compile-as-it-arrives, which is what this engine's
    decoder can honestly offer and is not observable through the API. The slice
    changed the `WebAssembly` namespace, so the wasm JS-API sweep was re-run:
    **1,001 tests / 0 fail** (§7).

  **Deliberately not in either slice, with the reason:**
  `CompiledWasmModule::get_wire_bytes_ref` and `source_url` (this engine decodes
  into structures and keeps no wire bytes, so retaining them would cost a copy
  per `Module` object across every wasm sweep for two methods nothing in the
  frontier uses); `WasmModuleCompilation` and `ModuleCachingInterface` (V8's
  *asynchronous* compilation and its cache-bytes protocol — a host that wants
  those wants V8's compiler); and `WasmMemoryObject::buffer` (nothing names it).

  **The first thing Slice A found: the engine's wasm support had never been in a
  host's build at all, and putting it there costs the host Cranelift.** The
  bridge's compiling the engine's wasm is what makes `v8::WasmModuleObject`
  exist, and `runtime/wasm` is a single feature covering both the JS API and, on
  native targets (through a `cfg(not(target_arch = "wasm32"))` table), the
  Cranelift codegen. Inside this workspace that is invisible; inside `deno/` the
  first `cargo check` after the bridge turned it on stopped at *resolution*,
  before any error could be counted: the engine's Cranelift 0.134.3 needs
  `target-lexicon ^0.13.5`, `libm ^0.2.16`, `arbitrary ^1.4.1` and `bumpalo
  ^3.20.2` where Deno's lock (its own Cranelift 0.117, through `deno_ffi`) has
  the older patches, and cargo will not move a locked patch version to satisfy a
  newly added member — it reports `failed to select a version` and writes
  nothing, so bumping one crate at a time cannot converge.

  **The alternative engine change was tried and is not available.** Splitting
  `wasm` into "the JS API and the interpreter" and "native codegen", with the
  native half as a *second* optional dependency on the same package (so that
  `default` could still mean codegen-on while a host asks for the JS API alone),
  is refused by cargo: `runtime` would then "depend on crate `wasm` multiple
  times with different names". Asking for `wasm` alone does give zero Cranelift
  (`cargo tree -p runtime --no-default-features --features wasm` shows none), so
  the split is *possible* — but only by moving the wasm32 carve-out into the
  `wasm` crate itself (its `compile` feature would have to become a no-op on
  wasm32 rather than a `compile_error!`), which is a much larger change to the
  engine's feature plumbing than this slice should carry. It is left open rather
  than half-done: recorded here, with the exact cargo error, so the next session
  does not try the same shape again.

  **What was done instead, named:** the host accepts the engine's Cranelift. Six
  leaf crates were bumped in `deno/Cargo.lock` — `target-lexicon` 0.13.2 →
  0.13.5, `libm` 0.2.8 → 0.2.16, and `arbitrary`, `bumpalo`, `gimli`,
  `regalloc2` to their newest compatible versions — which is a lockfile change in
  the *test subject* (untracked, and what any host integrating an engine with a
  Cranelift wasm JIT would do; V8's own crate brings a C++ toolchain for the
  same reason). No manifest in `deno/` changed. The cost is stated: the host's
  graph now carries two Cranelifts (its own 0.117 and ours 0.134) until it drops
  one, and a host whose lock predates those six patches must bump them. The
  engine's manifests are unchanged from what they were.
11. **The code cache and the unbound scripts — named before the bridge half is
  written.** Six sites, two shapes, and the first is the surprise: V8's code
  cache is *load-bearing* in `deno_core`'s default configuration. `ModuleMap`
  calls `create_code_cache` on every module load — a cache *miss* included
  (`libs/core/modules/map.rs:900`, guarded by `try_store_code_cache`, which the
  no-data branch sets `true`) — and turns `None` into
  `ModuleError::Concrete(UnboundModuleScriptCodeCache)`, a failed load; the CLI
  has caching on by default (`--no-code-cache` disables it,
  `cli/args/flags.rs:680`). So `None` is not available to this bridge even
  though it is the crate's own documented answer for a script that cannot be
  serialized.

  **Decision: `create_code_cache` answers `Some(CachedData)` whose bytes are the
  code's own source text.** The engine keeps no compiled form to serialize — it
  parses and evaluates in one pass — and V8's own contract makes the bytes
  opaque to a host, while `CachedData::rejected()` answers `true` here always
  (`crates/v8/script_compiler.rs`), so the flag keeps its meaning: a host that
  trusts it re-produces, and nothing is ever believed that was not checked. The
  cost is stated rather than hidden: a host's cache database holds a copy of the
  source and the round trip saves nothing. A cache that saves something means
  serializing the engine's compiled program, which §10 lists as engine-side
  build-order work — not something a bridge can fake into being faster.

  **Engine change, named — two exposures, no new state:**
  `api::Isolate::function_source(function)` reads the definition text
  `agent.ecma_functions` keeps — the text `Function.prototype.toString` answers
  with — and `api::Module::source_text()` reads `SourceTextModule.source`, kept
  for the same reason. Both are additive and read-only.

  The rest is bridge work: `Script::get_unbound_script` and
  `Module::get_unbound_module_script` (a retag — the engine's script handle and
  module record *are* the context-unbound forms, since a script here is its
  source text and a module is a record), the two unbound tags'
  `create_code_cache` and `get_source_mapping_url`, `Function::create_code_cache`,
  and an owned form for `CachedData` so bytes the bridge produced can outlive the
  call (`create_code_cache` answers `CachedData<'static>`).
  `get_source_mapping_url` is V8's own extraction — the magic comment
  (`v8/src/parsing/scanner.cc:280`, `//[#@]\s<name>=\s*<value>`, last one wins)
  — read as text, since the bridge has no lexer; §9 states the one word of
  divergence that costs.

  **Landed, 12 → 6** (§7 records it): the six sites resolve, six bridge tests
  guard the six mutations that would silently break them, and the engine's two
  exposures changed no behaviour (the battery ran anyway and reproduced every
  certified number).
12. **The stack-trace frames — the plan's named next engine item, and its survey
  splits it in two.** `StackTrace::current_stack_trace` (2 sites:
  `libs/core/ops_builtin_v8.rs:1518`, `libs/core/modules/import_graph.rs:164`) needs
a frame view of the running stack. §7's survey already split the subsystem: the
*frame accessor* over the execution contexts the engine keeps, and the
*per-activation source positions* every `StackFrame` line number would need. This
  item is the first half; the second is left open and unchanged by it.

  **What the engine has.** `Agent::execution_context_stack` holds one
  `ExecutionContext` per activation, innermost last: `function` (the closure, so
  a name when it has one), `script_or_module` (a `ScriptRecord` or a
  `SourceTextModule`), `realm`, and the running `source` text. What it does *not*
  have is a name for the code — a `ScriptRecord` has never recorded one, and a
  module record drops the specifier `parse_module` receives.

  **Engine change, named — one field, one table, one hook:**
  - `SourceTextModule` gains `name: Option<JsString>`, set at parse time:
    `parse_module` takes it as a parameter, `host_resolve_imported_module` passes
    the specifier it resolved the source under (a host-provided source is named
    by the name the host gave it), and `api::Module::compile_with_name` threads
    the host's own `ScriptOrigin` name — which is what the bridge's
    `compile_module2` has in hand and was discarding (it passed `""`).
    A *classic script* frame still has no name: naming one means threading a name
    through the eval path (`Context::eval` → `Agent::run_script` →
    `parse_script`), which every runner uses, and no site in the frontier asks for
    it — `deno_core`'s own code and user code are modules, whose frames are named.
    Recorded as an open item rather than half-threaded.
  - `Agent` gains `stack_traces: HashMap<usize, Vec<api::StackFrame>>`, keyed by
    the *box address* of the trace object the capture hands back — an ordinary
    object that exists so the entries have a liveness at all (the shape
    `wasm_modules` uses, keyed by the module object's id) — and
    `Agent::compact_weak_tables` prunes dead keys, which is what makes this table
    bounded where `error_stack`/`error_data` are not (§12 item 7).
  - `api::Isolate` gains `capture_stack(limit)`, `captured_frame_count(trace)` and
    `captured_frame(trace, index) -> Option<StackFrame>`, over a new
    `api::StackFrame` (function name, script name, line, column, and the four
    flags).

  **What is deliberately absent, and why the tier is honest rather than thin:**
  line and column are 0 — V8's own `Message::kNoLineNumberInfo` /
  `kNoColumnInfo` — because positions are the *other* half of the survey and the
  engine records none per activation; `is_eval`, `is_constructor` and `is_wasm`
  are `false` because nothing in an execution context records them (a direct
  eval's context inherits its caller's `script_or_module`, and the activation's
  `new.target` is not kept); and `is_user_javascript` is `true` for every frame,
  because the engine's Rust builtins never push a context at all — a frame here
  exists only for code the engine is running as JS. §9 states each.

  The bridge half: `crates/v8/stack_trace.rs`, with `StackTrace::current_stack_trace`
  and the frame accessors `deno_core` names (`get_frame_count`, `get_frame`,
  `is_user_javascript`, `get_line_number`, `get_column`, `get_script_name`,
  `is_eval`), a `Payload::StackFrame { trace, index }` variant (the shape
  `ModuleRequest` already has), and a script-name lookup that the engine test
  drives end to end.

  **Landed, 6 → 4, and the survey's premise was wrong.** The measurement is in
  §7 and it is the part worth reading: `execution_context_stack` is the spec's
  *execution context* stack, and the engine does not push a context per call, so
  a trace reports activations (script, module, eval, async/generator resumption,
  and the calls whose bodies need a spec context) rather than a call chain. The
  frame accessor shipped because "which code am I in" is answered exactly by it;
  "how did I get here" needs the VM's own frames, which is a
  performance-sensitive engine feature and is what §10's *structured frames* item
  now means.
13. **The heap statistics — named before the bridge half is written.**
  `HandleScope::GetHeapStatistics` (1 site: `libs/core/ops_builtin_v8.rs:1362`,
  `op_memory_usage`, which is `Deno.core.memoryUsage()`), and deno reads four of
  the crate's fifteen accessors: `total_physical_size`, `total_heap_size`,
  `used_heap_size`, `external_memory`.

  **What the engine has, and what it does not.** The heap is a chunked bump
  arena: `ArenaChunk { data, start, bump, end }` and a `for_each_live` walk the
  sweep already uses, so *committed* bytes (every chunk's buffer) and *live*
  bytes (the walk's live box footprints) are both readable without new state.
  What the engine has **no** notion of is V8's `external_memory` — bytes held
  outside the heap for the host's buffers — because an `ArrayBuffer`'s storage is
  an `Rc<Vec<u8>>` in `crates/byteblock`, outside the arena and outside any
  counter. What it *does* have is the engine's own record of every live buffer
  object: `Agent::buffer_data`, keyed by object identity, each with the
  `SharedBuffer` behind it. So the number is real rather than guessed.

  **Engine change, named — two read-only heap accessors and one api struct:**
  - `crux::heap::Heap` gains `committed_bytes()` (every chunk's buffer) and
    `live_bytes()` (the existing arena walk, summing each live box's footprint).
    Neither adds state and neither is on an allocation path.
  - `api::HeapStatistics` with the four fields deno reads, and
    `api::Isolate::heap_statistics()`: the arena's two numbers from `crux::heap`,
    and `external_memory` summed over `Agent::buffer_data` — skipping detached
    buffers, counting a shared block once (a block can back more than one buffer
    object), and *not* counting a borrowed block, whose bytes belong to the host
    (the `new_backing_store_from_ptr` path).

  **What the crate's other eleven accessors are: absent.** `heap_size_limit`,
  `malloced_memory`, `total_allocated_bytes`, `total_available_size`,
  `total_global_handles_size`, `used_global_handles_size`,
  `total_heap_size_executable` and the rest have no honest value here — the arena
  has no limit, the engine keeps no allocation total, and the JIT's code is not
  in the heap — so a host that names one gets a compile error rather than a 0
  (§9 records it). The four that land are exactly the four `deno_core` reads.

  **Landed, 3 → 2** (§7 records it): every number is one the engine already kept
  — the arena's committed and live bytes, and the agent's own record of live
  buffer objects for the external bytes, which are real because the engine's
  bookkeeping is, and stated as the engine's rather than V8's. Three tests, each
  mutated to prove it can fail; one of those mutations is what showed the first
  version of the external-memory test could not fail, so its subject changed
  (a view is not a buffer object) and the dedupe it was written for is gone.
14. **The module-graph tail — the engine half was written first, in the same
  session, so this entry is a record rather than an advance notice** (items 11-13
  were named before their engine halves existed; saying so here would be
  dressing it up). Two sites, and the engine change is additive rather than a new
  mechanism:
  - `api::Module::evaluate_for_import_defer` over a new
    `module::evaluate_for_import_defer` (`crates/runtime/src/module.rs`), which
    gathers the module's asynchronous transitive dependencies with the walk the
    deferred-import protocol already uses, resolves the deferred namespace
    immediately when there are none, and otherwise attaches the same countdown
    waiters the `.then` path attaches. `deferred_module_then`'s tail was split
    out into `await_async_dependencies` so both share it; that is a move, and
    the `.then` path's behaviour is unchanged (its own test still passes).
  - `api::Module::stalled_top_level_await_modules` over a new
    `module::stalled_top_level_await_modules`: a read-only walk over module
    status and the realm's link table, the shape `module_graph_has_tla` already
    has. No new state, no allocation path, and nothing written.
  - Bridge side: a `Payload::TemplateMessage { module }` variant (the shape
    `Payload::ModuleRequest` already has) for a message with no thrown value
    behind it, and the two methods over the above.

  **Landed, 2 → 0** (§7 records it), which is the end of the metric rather than
  of the work: the divergences the two sites carry are recorded below.
15. **Linking an import against a synthetic module — named before it is written.**
  Found by running rather than reading: `deno/`'s snapshot build now fails with
  `SyntaxError: Module ext:core/ops does not export op_log_debug`, and that module
  is one `deno_core` makes with `CreateSyntheticModule`.

  **The cause.** `resolve_export` (`crates/runtime/src/module.rs:3015`) answers a
  name through a module's `local_export_entries`, its `indirect_export_entries`
  and its star exports. A synthetic module has none of the three — its names are
  its host's declaration (`SyntheticModule::export_names`) and its values are the
  slots `SetSyntheticModuleExport` writes. The namespace path already knows that
  (`:2259` reads the names, `:3130` the values); the *link* path does not, so an
  import of a name a synthetic module declares resolves to nothing and linking
  reports the name missing.

  **What the change is, named:** the synthetic branch in `resolve_export`,
  answering the binding spec 16.2.1.5.2 gives a synthetic module's export — the
  module itself, bound to the name — together with whatever the import-binding
  path needs to read that binding instead of an environment slot.

  **Landed** (§7 records it), with the shape the first measurement settled: the
  bindings are the module's own environment, created at instantiation, so
  `resolve_export` answering `Local` for a declared name is all the link needed —
  the import-binding machinery (`create_import_binding`) was already there and
  reads an environment by name. `SyntheticModule::exports`, the slot table the
  namespace used to read, is gone rather than kept in step: one storage location,
  which is what the spec has, and its values are traced through the module's
  environment. As the entry said it would, nothing else in the module machinery
  moved.
16. **The snapshot format — named before it is written.** The engine-side item
  §10's build order has carried as (3) since the plan was written, and the one
  the record above left `deno/`'s build scripts failing on.

  **What the change is, named:** a new `crates/runtime/src/snapshot.rs` — the
  format, its walk and its reader — plus one accessor
  (`Intrinsics::name_of_value`, `realm.rs`) and two methods on `api::Context`
  (`write_snapshot`, `read_snapshot`). Nothing in `crux` moves: the walk reads
  and writes through the object model's existing surface
  (`ordinary_object_create`, `array_create`, `get_own_property`, `own_property_keys`,
  `define_property_key`, `intern`/`lookup`, `well_known`), which is why this is a
  new file and a name rather than a change to the heap.

  **Why the engine and not the bridge.** A blob's compatibility surface is the
  names it writes — an intrinsic name, a well-known symbol name — and the thing
  that has to mean the same in the writing tree and the reading tree is the
  engine's realm, not a bridge crate that is scheduled to be deleted. The
  bridge owns the *shape* (`StartupData`, `SnapshotCreator`, the V8 index
  conventions); the format is the engine's.

  **Landed** (§7 records it): what carries, what refuses by name, the versioned
  header, the index conventions, and the one thing the first version got wrong
  and the measurement caught — a root table built through an isolate's current
  realm rather than the realm being written.

  **Second half — the context table, named before it was written.** The table
  becomes structural (the header carries the context count, the body carries
  `(slot, item serials)` per context) instead of an array of arrays built as
  engine values, and each slot is written and read against *its own realm*:
  `encode_slots` takes a realm per slot, `decode_slot(realm, bytes, slot)`
  materializes into the realm being restored, and every record remembers the
  realm it was reached through, because that is the realm whose intrinsics
  recognize it. This is the engine change the one-realm refusal in the bridge's
  `add_context_data` was waiting on, so that refusal goes with it, and a value
  built in another realm is refused as such rather than surfacing as "a
  function" three frames later. What it does not do is still named in §7: a
  value shared between two contexts comes back one per context, the format
  carries no external references, no compiled code, no isolate data and no
  continuation.

  **Third part — external references, named before they were written.** The
  table the format indexes into: `encode_slots`/`decode_slot` take the host's
  addresses, an `External` is written as an *index* and never as an address, a
  pointer the table lacks refuses by name at build time, and an index the table
  lacks refuses with the index and the count at load time. `CreateParams::
  external_references` — carried and unused until now — is the table a restored
  isolate resolves against, and the creator's argument is the table
  `create_blob` writes into. A blob that passes `is_valid` and then cannot be
  read for the table it was handed is a host contract violation and aborts with
  the reason, which is what the crate we stand in for's loader does in the same
  situation. What it still does not carry is named with it: a *function* (a JS
  body needs the scope it was compiled in; a host callback here is a Rust
  closure rather than a pointer), compiled code, isolate data, continuation, and
  the things a host pointer points at — a template address is the host's to mean
  something again. *(The function half is superseded by the part below: a JS body
  is now carried as the source it is re-parsed from, and it is the **scope** a
  restore cannot rebuild — a host callback still refuses.)*

  **Fourth part — carrying a function, named before it was written.** The half
  the third part left refusing, and the blocker between `deno/` and a blob.
  `crates/runtime/src/snapshot.rs` gains `REC_FUNCTION` (a strict byte, the
  source as UTF-16 code units, the prototype serial, extensible, the property
  list), the walk descends into a function's object part, and
  `crates/runtime/src/function.rs` gains `instantiate_function_from_source` — the
  shared tail of CreateDynamicFunction's path, with [[Strict]] *supplied*, since
  a function strict by its enclosing context has no directive to re-derive it
  from. Reading a blob can now create a body, so `decode_slot`/`decode` and the
  builder take the agent mutably. A function reached as an *object* — which a
  prototype link makes possible — is folded back into the function value before a
  serial is assigned (`canonical`), or the walk would write an ordinary object
  where a function belongs. Still refused, by kind: a host callback (a Rust
  closure, not a name or an address a table holds), and a function the engine kept
  no source for. And what a restore cannot promise, and
  says in its own docs: an environment chain, so a free name in the body resolves
  through the reading realm's global. *A bound function was refused here too, and
  the next part carries it.* **The next kind is named by the measurement
  that follows this one in §7**: deno's `create_blob` now fails on a *bound
  function* reached from the module map it attaches at slot 1.

  **Fifth part — a bound function, named before it was written, and landed.** The
  kind the measurement above stops on. `crates/runtime/src/snapshot.rs` gains
  `REC_BOUND_FUNCTION`, whose record is the target, the bound `this`, the bound
  arguments, and the same prototype/extensible/properties triple a function gets;
  the walk descends into those three values *and* into the object part of the
  bound function itself. The restore calls `Function::bound_function_create` —
  the crux constructor the `bind` builtin already uses — and then defines the
  record's own properties, which is where a bound function's `length` and `name`
  come from rather than being recomputed from the target. Nothing in `crux`
  moves: `bound_function_create` is public already and takes no agent, so this is
  one file and no new engine surface. A bound function's *target* goes through
  the same walk, so a chain of binds round trips, an intrinsic target is written
  by name as before, and a target that is itself a host callback still refuses by
  name.

  **Sixth part — naming the realm's global function properties, named before it
  is written, and landed.** The kind deno's `create_blob` stopped on: `isFinite`,
  a standard global function the realm installs but did not name. The measurement
  and its two probes are in §7's record above — the refusal is *not* a bind's
  target, and the walk reaches the realm's **global object** on the way. The
  change is the one `%eval%` already models (`realm.rs:509`): each of the global
  function properties `builtins/global.rs::install` puts on the global object
  (`isFinite`, `isNaN`, `parseFloat`, `parseInt`, `encodeURI`,
  `encodeURIComponent`, `decodeURI`, `decodeURIComponent`, `escape`, `unescape`)
  is now registered under the spec's own well-known name (`%isFinite%`, ...),
  which the spec's intrinsic table does define, so a reference to one is written
  by name and resolved in the reading realm that rebuilds them — no callback is
  carried for them, and the format's compatibility surface stays "the names it
  writes".

  **Second half of the same part — the error realm the registration exposed,
  named before it was fixed.** Registering these made `owning_realm` answer for
  ten more builtins, and the corpus showed what that reached:
  `construct_inner`'s cross-realm block was converting *any* kind-only error from
  a cross-realm callee into that callee's realm, including EvaluateNew's
  "not a constructor" TypeError, which the spec throws in the **caller's** realm
  (`language/expressions/new/non-ctor-err-realm.js`, 0 fail to 1 fail on the
  first battery run). It is now gated on `is_constructor`. Same part, because the
  measurement that named it is the registration above — and it is a fix at the
  root rather than a workaround for it, which is why the registration stays.

  **Seventh part — carrying a class constructor, designed before it was written,
  and landed.**
  What deno's `create_blob` stops on, and what the probes say it is: a **base
  class** whose constructor is the *implicit* one (`constructor_kind` base,
  `default_derived` false, no `super_constructor`, no parameters, no body
  statements), carrying **four instance fields** and a **private environment**
  — and whose prototype object's own keys are exactly `["constructor"]`, so the
  class has **no methods**. That last fact is what makes this one step rather
  than two: the class's prototype and static sides are already carryable by the
  walk as an ordinary object graph, so what is missing is the constructor alone.

  *What the source is, per the spec, and why that is the whole design.*
  ClassDefinitionEvaluation sets `ctorFunc.[[SourceText]]` to the **class source
  text** for the explicit *and* the implicit constructor — the set is a sibling
  of the empty/non-empty branch, not inside it (spec.html:25789-25795) — and
  `Function.prototype.toString` answers `[[SourceText]]` when it is not empty
  (spec.html:31977-31978). The engine captures the explicit constructor's
  *method* span today, only as a body-cache key, and stores **no** source for the
  record: so a class constructor answers the native form, which the four
  `builtins/Function/prototype/toString/class-*-ctor.js` fixtures cannot see
  because `assertToStringOrNativeFunction` accepts that branch. So the record's
  source is not a convenience — it is the spec's own slot, and the same slot the
  function record already carries.

  *The design as landed: the record carries `[[SourceText]]`, the restore parses
  it as a class expression.*

  - **Engine.** `class.rs`'s
    ClassDefinitionEvaluation captures the **class** source once
    (`capture_source(agent, class.span)` — the helper functions use, and `Class`
    carries the span) and threads it through `instantiate_class_constructor` /
    `instantiate_class_constructor_with` / `instantiate_default_derived_constructor`
    as a new `source: Option<JsString>`, for **both** branches. Two knock-on
    effects, both refinements rather than risks: a class constructor's
    `Function.prototype.toString` then answers the class source, which is what
    the spec requires, and the implicit constructor gets a source for the first
    time. The explicit branch keeps using a source for `shared_function_body`'s
    cache key, and it is the constructor *method*'s span as before — the class
    source reaches only the constructor's own record, so the body cache is
    untouched.
  - **Format.** `REC_FUNCTION` gains a **kind byte** after its tag
    (`function`-expression vs `class`-expression) rather than a second tag: the
    value kind is the same (a function), the only difference is the grammar its
    source is read in, and the rest of the record — strict, prototype,
    extensible, properties — is the same shape with one reader and one writer. A
    separate tag would mean two tags for one value kind and either a duplicated
    tail or a shared helper that makes the tags lie about being different shapes.
    The strict byte is written `true` for a class (a class body is always strict)
    and the class path does not read it, because the grammar decides strictness.
    A layout change is part of v1: nothing outside this tree has ever written a
    blob in this format.
  - **Restore.** For a class record, evaluate `(<class source>)` as a class
    **expression** in the reading realm's global environment — the same
    bootstrap-context trick the function path uses, and an expression so the
    class name is bound inside the class rather than in the global scope — take
    the completion value as the constructor, then apply the record's
    prototype/extensible/properties exactly as a function does. Everything a
    class constructor is made of then comes from the engine's own
    ClassDefinitionEvaluation: `constructor_kind`, the home object, the prototype
    object, `fields`, the private names and their environment, and the
    constructor's `[[IsClassConstructor]]`. That is the same code that built it,
    which is the safest reconstruction available and the reason no class part is
    carried.

  *The alternative that was rejected, and why.* A structural record — kind,
  home object, `super_constructor`, fields with their initializer functions,
  private names, and a new engine entry point to assemble a class constructor
  from them — would avoid re-running definition-time code. It was rejected
  because it duplicates ClassDefinitionEvaluation's constructor half into the
  deserializer, needs the private-name/brand machinery to be reconstructible
  from a record (identity that today only the class-evaluation path creates), and
  gives the engine a second way to make a class constructor — two paths that must
  then agree forever on every future spec change. The measured class needs none
  of that: base, no heritage, no methods, four fields.

  *What it promises, and what it does not.*

  1. A class constructor whose class source the engine kept. The source is
     captured at class instantiation under the same condition a function's is
     (the enclosing execution context has source text), so a class created where
     a function would have none refuses the same way — "a class constructor (the
     engine kept no source text for it)", the refusal already in the tree.
  2. **The class's definition-time code runs again at restore**: computed
     method and field keys are re-evaluated, and a `static {}` block or a static
     field initializer *executes*. Per-instance field initializers do not, and
     neither do method bodies. V8's snapshot restores objects without re-running
     the definition, so this is a real divergence; it is stated in the module
     docs and here.
  3. The class's **environment** is the reading realm's global: `extends <expr>`
     re-resolves there, and so do the initializer and method closures the class
     evaluation creates. For `extends` this is the same limit the function record
     already states for free names, but **harder**, because it fails at restore
     (as `UnrebuildableFunction`) rather than at call time. The measured class is
     a base class and does not reach it.
  4. The record is a function-shaped record, not a "class unit": the walk still
     descends into the class's own graph (its `prototype` property, its static
     side), so a host that mutated the class keeps what it added. This is why the
     design does not simply carry the class source and skip the subgraph.
  5. A class **with methods** is not carried by this step: the walk reaches the
     prototype object's method properties and refuses them as methods. Methods
     are the eighth part, and they are a different mechanism — a method's source
     is method-form (`m() {}`) and needs a parse context of its own plus its
     carried `[[HomeObject]]`; the same context is what an accessor
     (`get x() {}`, source-less when this was written) needed — part 17 closed
     that, and the accessor is carried by the same method record now.

  *Acceptance tests — all landed.* Engine: a class constructor round trips and
  constructs (`new restored().x` answering a field's value, and a bare
  `restored()` throwing because `[[IsClassConstructor]]` came back); a class whose
  public field reads its own **private** field (`class C { #secret = 41; x =
  this.#secret + 1; }` → `new restored().x === 42`), which proves the private
  environment and the initializer were both rebuilt and needs no method to
  observe; the restored class's `Function.prototype.toString` answering the class
  source; and the fresh class's too, which is the conformance half the capture
  change buys and which fails before it. Bridge: a class constructor attached as
  context data, restored, constructed from a script, with
  `restored.prototype.constructor === restored`. The refusal the part before this
  one left in the tree — `a_class_constructor_is_refused_by_what_it_lacks` — is
  positive coverage now, and the branch it guarded stays covered by a re-aim:
  `(new Function('return class C { x = 1; }'))()` defines a class where the
  enclosing execution context has no source text, and it refuses exactly as the
  old test said.

  *Mutations.* A wrong grammar byte (writing `Grammar::Function` for a class)
  fails both round-trip tests with the parser's own error; dropping the capture
  fails three, including the fresh-class `toString` test on `function C() {
  [native code] }` where the class source belongs. The third named mutation — the
  class branch evaluating without the reading realm's environment — is **not
  observable** in this harness, for the reason §7's record states: the api makes
  the last context created current and every test has to eval in the reading
  realm, so no test here can tell two realms apart at restore time. What is
  caught instead is the push/pop pairing itself, which leaves the agent with no
  running execution context when it is unbalanced.

  *Gates, all met* (§7's record): `cargo test -p runtime --lib` 835 passed / 0
  failed, `cargo test -p v8 --features simdutf --lib -- --skip
  the_data_a_built_function_carries_survives_a_collection` 220 / 0 / 1 filtered,
  the workspace 5,208 / 0 /
  4 ignored, `cargo fmt --all -- --check` and `cargo clippy --locked --workspace
  --all-targets -- -D warnings` clean, and the full battery unchanged — test262
  `all` 48,464 pass / 0 fail / 0 crash / 0 hang (158 skip), `intl402` 3,205 / 0
  (152 skip), the eight wasm core suites 64,594 checks / 0 fail / 0 pending, the
  JS-API sweep 1,001 / 0. The four `class-*-ctor.js` toString fixtures pass
  either way, which is why the fresh class's source is pinned by a test of ours.

  *And what it exposes, now measured.* The next blocker is a **host callback** —
  an unnamed `FunctionKind::Builtin` reached through the attached graph — which
  is the `function` field of the external-reference table, the mechanism the
  format already uses to write a host *pointer* by index. The method machinery of
  (5) is still open, and is still the eighth part.

  **Ninth part — carrying a host callback, designed before it was written, and
  landed before the eighth part because the seventh part's measurement named
  it.** The
  value the seventh part's measurement stops on: a built-in function with no
  name and no own `name` property, i.e. one the *host* built and the engine holds
  as a Rust closure (`FunctionKind::Builtin`). The engine cannot rebuild that on
  its own — the body is a closure, not text — so this is the first part whose
  answer needs the **host's** cooperation rather than one more record the engine
  can write alone.

  *What the crate we stand in for does, and why it is the same table.* V8 keeps
  the callbacks of a host function as addresses and its serializer rewrites each
  one into an `ExternalReference` index (`ExternalReferenceEncoder`), because the
  address is a property of the process that loads the blob rather than of the
  data — the exact statement `crates/v8/external_references.rs` already makes for
  host pointers. `deno/libs/core/runtime/bindings.rs:63-75` is that table in deno:
  `call_console`, `import_meta_resolve`, `catch_dynamic_import_promise_error`,
  `op_disabled_fn`, the callsite helpers, and one entry per registered op. So a
  callback is **not a new mechanism**: it is the index the format already writes
  for a host pointer, plus the function's own object part.

  *The design: one new record, two host-facing parameters, and no engine state.*

  - **Format.** `REC_HOST_CALLBACK` (tag 16): the external-reference **index**, the
    **data serial** (the value the host built the function with, `NO_REF` when it
    built it with none), the
    prototype serial, extensible, and the same property list every other record
    with an object part carries. A separate tag rather than a fourth grammar byte,
    because the carrier differs in *kind* (an address versus source text) and not
    in grammar — the line the seventh part's grammar byte draws, drawn the other
    way here. The index is found with the **existing** `external_index`, so a
    callback the build's table does not hold refuses with the same message an
    `External` value gets, and an index the load's table does not hold is
    `ExternalIndexOutOfRange`. The data is not an afterthought: it is a value the
    callback reads, so a record that dropped it would restore a function that
    quietly does something else — and deno builds *every* op that way
    (`bindings.rs:1008`, `.data(external.into())`).
  - **The walk.** `callable()` gains the host. A builtin the host recognizes is
    `Callable::HostCallback { pointer, data }`, and the walk visits the data like
    any other value it carries, so the data's own graph is written once and
    referred to by serial; a builtin it does not recognize refuses
    exactly as today, which is why the engine's own refusal test keeps passing
    unchanged.
  - **The host.** `runtime::snapshot::HostCallbacks`, two methods with one
    meaning — *the host's own callbacks*: `callback_of(&self, &Handle<Function>)
    -> Option<HostCallback>` (the table entry and data a function was built
    from, `None` for a builtin the host did not make) and `callback_at(&self,
    usize, Option<api::Local>) -> Option<api::FunctionCallback>` (the call the
    load's table holds at an index, with the data the blob carried, in the
    engine's own callback shape). `encode_slots`/`decode_slot` and
    `api::Context::write_snapshot`/`read_snapshot` take it as
    `Option<&dyn HostCallbacks>`: **`None` is today's behaviour**, so the format's
    own tests and the engine's refusal need no host and no change.
  - **The restore** builds the function through the engine's own
    `api::template::host_function` — the helper a materialized `FunctionTemplate`
    already creates its functions through (**`pub(crate)` now, one word**) — so a
    restored callback's call view, return-value slot and pending-exception
    translation are the engine's existing ones rather than a second
    implementation of them. The record's prototype is passed in, so the function
    is born with it, and the record's own properties are then defined exactly as
    they are for a function: `name`, `length` and anything a host set come back
    as written, not recomputed.
  - **The bridge** records what it built each host function from — the callback
    address and the data, by the function's identity — where it materializes it
    (`crates/v8/template.rs`), and answers the engine from there. The address is
    stable because `map_fn_to()` monomorphizes the adapter per host function type,
    which is why the same `call_console.map_fn_to()` is both the table entry deno
    registers and the callback the function was built with.

  *What it promises, and what it does not.*

  1. A callback whose pointer is in the **build's** table, restored against the
     pointer the **load's** table holds at that index — which is the whole point:
     the address belongs to the process, not to the blob.
  2. **A restored host callback has no `[[Construct]]`.** Its construct behaviour
     is the host's *template* (V8 builds the instance, applies the instance
     template, and decides what the callback's return value means), and a blob
     carries none. So a builtin the host recognizes as **constructible** is
     refused as *a host constructor*, naming that reason: writing the call half of
     something whose construct half cannot come back is exactly the "read back
     wrong" the format refuses everywhere else. `api::template::host_function`
     builds the call half only, so this is also what makes the promise true rather
     than merely stated. Deno sets the behaviour per op
     (`op_ctx_constructor_behavior`, `bindings.rs:800`: `Allow` only for an op
     declared constructable), so the ops and `call_console` are the non-constructible
     kind — and a constructable op would be refused by name rather than restored
     without `new`.
  3. **The data is carried, and it is a value**: a restored callback reads what
      the host attached when it built the function, or `undefined` when it
      attached none. That is the one place this part is *stronger* than V8's
      model, and it has to be: the data is not the template's, it is a language
      value the callback reads, and a record that dropped it would restore a
      function that runs a different branch of the host's code.
  4. A **bound function over a host callback** is carried by this too, because a
     bind's target goes through the same walk — an engine test now reaches one
     and calls it. The engine's existing
     `a_bound_function_over_a_host_callback_is_refused_by_name` still refuses,
     because it supplies no host; the *bridge* test that used to end the build on
     a callback became the table refusal, which is the failure a host with an
     incomplete table actually gets.
  5. **A hand-built engine builtin is still refused.** A host that never registers
     a callback (the engine's own test builds one with `Function::create_builtin`)
     gets today's message, because the refusal is the *host's* not answering, not
     the engine's inability.

  *Acceptance tests — all landed.* Engine, through a fake host the test owns: a
  callback whose pointer is in the table round trips, takes the **index's**
  pointer back (a two-entry table, where an ignored index and a used one differ —
  the trap §7's external-reference record already paid for once), comes back
  callable, and answers what the restored callback returns; a bind whose target is
  that callback round trips and is callable too; the data the host built it with
  rides with it, asserted on the value the restore handed the host; a pointer
  absent from the table is refused by name; a constructible callback the host
  recognizes is refused as *a host constructor*; a blob naming a callback read by
  a load that supplied no host is `NoHostCallback`; and without a host the
  engine's own refusal is unchanged. Bridge: a `FunctionTemplate`'s function with
  `ConstructorBehavior::Throw` and a `.data(...)` value, attached as context data,
  restored, and called from a script — `restored()` answering the data, which is
  the shape deno builds every op with.

  *Mutations, each caught.* Writing index 0 instead of the entry's (the two-entry
  table answers the other pointer); not consulting the host (the old refusal
  returns); dropping the record's prototype from the restore (a null
  `[[Prototype]]`); dropping the record's own properties (a missing `name`); and
  writing `NO_REF` for the data, and ignoring it at restore, each of which fails
  both the engine's data test and the bridge test.

  *Gates, all met* (§7's record): `cargo test -p runtime --lib` 840 passed / 0
  failed, `cargo test -p v8 --features simdutf --lib -- --skip
  the_data_a_built_function_carries_survives_a_collection` 221 / 0 / 1 filtered,
  the workspace 5,214 / 0 / 4 ignored, `cargo fmt --all -- --check` and `cargo
  clippy --locked --workspace --all-targets -- -D warnings` clean, and the battery
  unchanged — test262 `all` 48,464 pass / 0 fail / 0 crash / 0 hang (158 skip),
  `intl402` 3,205 / 0 (152 skip), the eight wasm core suites 64,594 checks / 0
  fail / 0 pending, the JS-API sweep 1,001 / 0. One workspace run showed a single
  failure before those; two re-runs were clean and the test is the documented
  flake `certified_body_global_read_fast_path_stays_spec_exact`, which passes on
  its own.

  *And what it exposes, now measured: deno's build moves by exactly one value.*
  `create_blob` **carries `Deno.core.callConsole`** — the probe printed `carried
  index=0`, and `bindings.rs:63` is entry 0, so the callback the seventh part's
  measurement stopped on is the one this record carries — and then stops on the
  **next** callback: a pointer the host's 426-entry table does not hold, i.e. one
  `create_external_references` never registers. Deno builds those during the
  bootstrap's module evaluation — `Function::builder(closure).data(...)`
  callbacks in `deno/libs/core/modules/map/{evaluation,dynamic}.rs` — so the next
  step is **host-side rather than another record**: those callbacks have to be in
  the table the host hands the creator. Whether V8 itself requires that, or
  encodes an unknown callback as a null reference and lets the host replace it,
  is not in this checkout; that is the one question this measurement leaves open,
  and it is recorded rather than guessed.

  *And what it still exposes.* The construct half of (2) — the template a host
  restores alongside its table — and the entry-retirement question §12 item 7
  already records for the bridge's position table, which the bridge's new
  function→callback table shares.

  **Tenth part — carrying an arrow, designed before it was written, and landed.**
  The kind deno's build stopped on next: a function with no `[[SourceText]]`.
  The engine kept none for arrows — `instantiate_arrow` built its record with
  `source: None` — though the spec sets `[[SourceText]]` for the ArrowFunction
  production exactly as it does for a class, and `Function.prototype.toString`
  must answer it. So this part is two halves, and the first is a conformance fix
  the engine owed whether or not a snapshot ever carried an arrow.

  - **Capture.** `instantiate_arrow` takes the arrow's span and stores
    `capture_source(agent, span)` in the record, so an arrow's `toString` answers
    its own text (`(x) => x`, not `function g() { [native code] }`); the span
    travels to it through `Step::CreateArrow` (the AST node is decomposed into
    params and body there, so the step carries the span) and through the
    interpreter's and the JIT's helpers. An existing engine test **pinned the
    gap** — `function_to_string_renders_source_or_native` asserted the native
    form for an arrow — and now asserts the source, which is the clearest
    evidence the change is a fix rather than a preference.
  - **Format.** `GRAMMAR_ARROW` (byte 2). An arrow's source is an *expression*, so
    `parse_function` cannot read it, and the restore **evaluates** it — the class
    record's path, which is why `build_class_function` became
    `build_evaluated_function(source, proto, grammar)` and serves both. The walk
    tells them apart by `[[ThisMode]]`: lexical for an arrow and for nothing else
    in this engine.

  *What it promises, and what it does not.* An arrow's kind survives — an async
  arrow's `[[Prototype]]` comes from the evaluation and from the record alike, and
  a restored arrow still has no `prototype`. What no restore can give back is the
  scope the arrow closed over: its `[[ThisMode]]` is lexical, so a restored
  arrow's `this` and its free names are the **reading** realm's global. That is
  the function record's free-name limit, and it is harder here because capturing
  `this` is what an arrow is for; the module docs state it.

  *Tests.* Three in `crates/runtime/src/snapshot.rs` — a round trip that is
  callable and still has no `prototype`, a fresh arrow answering its source (the
  conformance half, which fails before the capture), and an async arrow keeping
  its kind (`%AsyncFunction.prototype%`, and a call still answering a promise) —
  plus one in `crates/v8/snapshot.rs`. The refusal test's arrow case is re-aimed
  at a closure with no text at all (`(new Function('return () => 1'))()`), because
  an arrow is positive coverage now. Two mutations, each caught: an arrow written
  with the function grammar fails both round trips as a parse error, and
  `source: None` in `instantiate_arrow` fails all three arrow tests, the
  conformance one answering `function () { [native code] }`.

  **And a bridge-side fix that needed no part of its own: the bridge's console.**
  With host callbacks carriable, deno's `create_blob` stopped on `console.log` —
  **the bridge's** console, installed on every context `Context::new` makes,
  which no host ever had a chance to register, so its callback was in no
  external-reference table. The bridge now puts it in the table itself
  (`snapshot::engine_table`), on the write and the read side, and **first** rather
  than last: a host's table is legitimately a different length at write and at
  load (deno externalizes its lazy sources only while snapshotting), so a
  position that depended on that length would name one entry when the blob was
  written and another when it is read. One entry covers every console method,
  because they are one callback under many names.

  **Eleventh part — the text a call frame is running, designed before it was
  written, and landed.** The blocker the tenth part's measurement named: `async_op_0`, a
  closure created inside a function the **host** calls from Rust, where no frame
  beneath the callee carries a source, so the closure has no `[[SourceText]]` at
  all. The engine half is one slot per call frame; the format does not change.

  - **The slot is the *parse* text, not `[[SourceText]]`.** `EcmaFunction.source`
    is what `Function.prototype.toString` answers: the function's own **slice**
    of some larger text. A body's AST spans are not offsets into that slice —
    they are offsets into the text it was *parsed from* (the script, the module,
    a `Function`-built body, an eval) — so a frame that carried the slice would
    either fail `enclosing_source`'s fit check at a body span past its own end,
    or, before the check, answer a wrong slice of an unrelated text. The record
    therefore carries `parse_text: Option<JsString>` beside `source`, and it is
    exactly what `enclosing_source` resolves.
  - **Where it comes from.** `register_function` fills it from the same walk
    `capture_source` already uses (`enclosing_source(agent, body.span)`), falling
    back to the text the caller supplied — CreateDynamicFunction and the restore
    parse the very string they pass as `source`, so there the two facts coincide
    — and the module's declaration pass, which registers under a **bootstrap**
    context with no source of its own, sets it from the module text it already
    slices `[[SourceText]]` out of, the way it already patches
    `declaring_module`. `instantiate_arrow` computes it for the arrow's span, the
    span it captures `[[SourceText]]` from.
  - **The call frames.** The certified call and construct, the certified and slow
    tail paths, the slow call and construct, and the async, generator and
    async-generator calls each push the callee's `parse_text` where they pushed
    `None` (the certified pair) or the **caller's** text (the rest — right only
    when the two texts happened to be the same one, and a silent wrong slice in a
    `toString` when they were not). The slot stops being "never consulted": the
    certified push's comment and `enclosing_source`'s doc say what it is for now
    — every closure the body creates, and the body-cache key `source_hash_at`
    derives from it.
  - **What it makes visible, and why that is the honest reading.** `is_frame`
    counts a context with a source as a frame, and it was written when a source
    meant "a script, a module, or eval": the criterion was a *proxy* for "code,
    not the realm's bootstrap context". A certified call is the only frame an
    ordinary call has, and it is now a real activation, so a trace reports one
    frame per call rather than only the calls that read a spec-only slot. The
    residual gap is a **leaf-inlined** call, which pushes no context by design (a
    leaf body creates no closures and reads no spec-only slot — both are
    disqualifiers of leafhood), and it cannot be observed from JavaScript at all:
    reaching a trace means calling, and a call disqualifies leafhood. What the
    new frames do **not** carry is a **name** — the certified push keeps
    `context.function` gated on its one reader, a sloppy body's mapped
    `arguments` — so `get_function_name` answers `None` for them, and
    `Error.stack`, which names frames from that same slot, is unchanged. Naming
    them costs a value store on the call path and is a separate step, recorded
    rather than taken here.

  *Acceptance tests.* Engine: a closure created inside a function the **host**
  calls keeps its source (`function_source` answers its own text rather than the
  native form), and — the shape deno's walk actually refuses — that closure is
  **carried by a snapshot** and comes back callable, which is what fails when the
  creation site has no text. A second test pins the other half: the frame carries
  the **callee's** text, not the caller's, when two scripts differ — a script's
  factory called from a *second*, differently-sized script answers the closure's
  own text, where the inherited caller's text fails the fit check and answers the
  native form. Bridge: the stack-trace test's expectation moves with the
  measurement.

  **Eighth part — carrying a method, designed before it was written, and landed.**
  Named in the seventh part and deferred twice, because the tenth and eleventh
  parts were the kinds the measurements reached first. A probe on the walk's
  method refusal says what the value is: `__setTickInfo(buf) {tickInfo = buf;}`, an
  ordinary object-literal method, **strict**, not async and not a generator, with
  an object home object and no `name` on the record (its `name` is an own
  property, which the record's property tail already carries). The engine has kept
  its `[[SourceText]]` all along — a method's `toString` answers `m() {}` — so
  this is the first part whose text the engine has and whose *grammar* it lacks.

  - **What the spec says the source is, per kind.** MethodDefinition,
    GeneratorMethod, AsyncMethod and AsyncGeneratorMethod each set
    `[[SourceText]]` to the source text matched by that production — the property
    name and the parameter list, with no `function` keyword
    (spec.html:24626-24715) — which is what the engine captures for a method
    today. An **accessor** is a MethodDefinition alternative
    (`get ClassElementName ( ) { FunctionBody }`), so its source is
    `get x() {}` *including* the keyword (spec.html:24653-24670) — and that is the
    half the engine keeps nothing for, because the AST nodes for `get`/`set`
    carry no span covering the keyword. It is the **second half** of this part.
  - **The parse entry had been doing the wrong thing quietly, and the mutation
    sweep is what showed it.** `parse_function_expression` consumes the
    `function` keyword *without checking it*, so `parse_function` reads a
    method's `m() {}` as an *anonymous* function with the keyword's place taken
    by whatever came first — the grammar refusal's own diagnosis ("it cannot be
    re-parsed on its own") was not what the parser did. `parse_function_with_async`
    now checks for the keyword first, which is the premise the grammar choice
    rests on and the reason the method grammar is not merely a spelling of the
    function one: reading a method as a function loses `[[HomeObject]]` (so
    `super` cannot resolve — a body containing it is refused outright, as an
    early error: `super is only valid inside class methods`) and gives the
    closure the deferred `prototype` a plain function gets, which a method must
    never have.
  - **Format.** `GRAMMAR_METHOD` (byte 3), and — a method being the one callable
    whose `[[HomeObject]]` is part of what makes it one — a **home-object serial**
    in `REC_FUNCTION` when the grammar says so, `NO_REF` for a method the engine
    kept none for. The serial follows the strict byte, so the layout stays one
    reader and one writer with a grammar-conditional field; a layout change is
    still part of v1.
  - **The walk.** `callable()`'s method refusal becomes `Grammar::Method` carrying
    `data.home_object`, and `visit` descends into the home object like every other
    value a carryable's state is made of, so a home object the format cannot
    carry refuses by name exactly as any other edge does. A *synthesized*
    method-form body — a static block's closure, a field initializer's — has no
    method-definition text; it is carried as a method too and refuses at
    **restore**, as `UnrebuildableFunction`, the class and arrow records' own
    precedent for a source that will not evaluate. Neither is reachable from a
    host's graph: a class's field records are not carried, and a static block's
    closure is nobody's property.
  - **Restore.** Evaluate `({<source>})` — the object literal a MethodDefinition
    is only valid inside, and the class and arrow records' path, so
    `build_evaluated_function` gains the method case — and take the temporary
    object's **first own property descriptor**: a data property's value is the
    method, an accessor property's `[[Get]]` or `[[Set]]` is the getter or the
    setter, which is what lets one path serve all three forms. Then
    `function::make_method(closure, home)` puts the record's home object back over
    the temporary one, which is the whole reason the serial exists. The
    prototype/extensible/properties tail is unchanged, and the restored method's
    own `[[SourceText]]` is re-captured from the evaluation — the slice of the
    wrapped text at the method's span — so it answers `m() {}` and a second round
    trip is stable.
  - **Strictness, which the evaluated grammars had been losing.** A class method
    is strict whatever its body says and an object method inherits the strictness
    of the code around it, so the record's `strict` byte is not decoration: the
    restore wraps the source in **strict code** when the record says strict
    (`"use strict"; ({<source>})`) and in plain code when it does not. The span
    arithmetic is unaffected — the wrapper shifts the text the method's own slice
    is taken from, and the slice is still the method — and it closes the same gap
    for the **arrow** record, which had been restoring a strict arrow sloppy since
    the tenth part.

  *What it still does not carry.* An **accessor**, until its half lands (the AST
  span for the `get`/`set` keyword). A method's definition-time code: a computed
  property name is re-evaluated at restore, the class record's stated divergence.
  And a method's **scope**: like every function record, a free name in the body
  resolves in the reading realm's global, which is where the measured deno
  method's `tickInfo` will resolve rather than to the module that closed over it.

  *Acceptance tests — all landed.* Four engine: an object-literal method round
  trips, is callable, answers its own source and has **no** `prototype`; a method
  reads `super` through a home object set by `__proto__`, which is the test the
  serial exists for; a class **with** a method round trips, constructs and keeps
  its method strict (an undeclared assignment throws after the restore) while an
  object method written in sloppy code comes back sloppy; and a **strict arrow**
  comes back strict while a sloppy one stays sloppy, which is the wrapper's other
  half. One bridge: a method attached as context data, restored, reaching `super`
  from a script. The refusal-by-kind test is re-aimed to the **accessor** alone,
  because a method is positive coverage now. Eight mutations, each caught: the
  method grammar read as the function grammar, the home serial not written, not
  applied, not walked, read but ignored, the wrapper never strict, the wrapper
  always strict, and the parse entry's keyword check.

  **Twelfth part — the realm's own builtins by name, designed before it was
  written, and landed.** The value the eighth part's measurement named:
  `Math.abs`, a builtin the *engine* made, a value the host's table legitimately
  cannot hold because the engine made it. It is reached as a value rather than
  through `%Math%` (which the walk stops at, being named) because deno's
  `00_primordials.js` copies the realm's builtins into its primordials object, so
  the walk can meet *any* builtin the realm installs.

  - **The mechanism is the sixth part's, applied past the ten globals.** That
    part named the global object's ten function properties; this one names what
    those objects and every other intrinsic object and function *hold*.
    `Intrinsics::name_members()` runs last in `initialize_host_defined_realm`'s
    bootstrap, after every install has declared its names, and derives a name for
    every **function** member of every registered owner: `%<owner-stem>.<key>%`
    for a data property, `%get …%`/`%set …%` for the halves of an accessor, and
    `%<stem>[Symbol.x]%` (no dot) for a well-known symbol key — each the spelling
    the installs that declare a member already use. Measured: **355 names** on a
    realm this engine boots.
  - **Derived, not declared, because the derivation is the shared fact.** The
    writing and the reading realm both rebuild the same installs, so the owner's
    registered name plus the member key is exactly what the two realms agree on
    without a per-site name. The rules that keep it sound: the table is read into
    a scratch `Vec` before anything is written (`define` takes the `entries`
    borrow); an existing name is left alone, so a derivation never displaces a
    declared one; and only functions are named — naming a random object would be
    a false intrinsic claim. `name_function` lifts the plain registration out of
    `define` so a derived name does no dispatch-handler lookups; it cannot need
    one, because a member's call reaches its handler through the names its
    *install* declared and the name table is per function.
  - **And the pass was not idempotent, which a probe found.** The pass's own doc
    claimed a second run is a no-op; counting what each call added printed
    **355, then 120**. The second call scanned the first call's derived names as
    owners — `%Array.prototype.constructor%` is one pass's spelling of `%Array%`
    — and minted second-order names (`%Array.prototype.constructor.from%`) beside
    the declared `%Array.from%`. The pass now remembers the names it derived and
    skips them as owners, so the documented claim is true and a caller may run it
    twice, which the test does to reach the accessor branch.
  - **What it promises.** A named builtin comes back as the **reading realm's
    own** value — the same function object, not a copy — because the restore
    resolves the name in the realm it reads into. Deno's `uncurryThis` shape
    (`Function.prototype.call.bind(Math.max)`) has a bind's *target* that is a
    named builtin, so this is what makes that record resolve.

  *Acceptance tests.* Two, both landed: `realm::tests::
  a_realm_names_the_members_of_its_own_builtin_objects` (naming by stem and key,
  the accessor prefix, no displacement, idempotence) and `snapshot::tests::
  a_builtin_member_round_trips_as_the_realms_own` (Math.abs by name with the blob
  asserted to spell it, a bind over `Math.max` restored and called, and a
  constructor's static `Number.isFinite`, which no install names of its own
  accord). Six mutations, each caught: the pass not run, the stem keeping its
  percents, accessors not named, function owners not scanned, the owner dropped
  from the name, and the idempotence skip.

  **And the format now builds and loads.** With this part, deno's `create_blob`
  completes — the example's build script runs to the end and writes a
  226,426-byte blob, the first time it has. Reading it back, which had never been
  exercised, restores contexts until it reaches a **method whose body reads a
  private name** and panics: the object-literal context the method record
  evaluates in cannot bring a `#name` into scope. That is part 13.

  **Thirteenth part — a method that reads a private name is carried as a member
  of its class, designed before it was written, and landed.** What the load
  stops on: an
  ordinary class method, `enter(value) { const previousContextMapping =
  getAsyncContext(); this.#enterWithPreviousContext(value,
  previousContextMapping); ... }` — a `#privateMethod` call in the body. The
  method record evaluates its source in the object literal a MethodDefinition is
  only valid inside, and a `#name` is in scope only inside a **class body**, so a
  method that reads one cannot be rebuilt that way. Nor may it be rebuilt by
  evaluating the class source *again*: a private name belongs to one class
  declaration, so a second evaluation mints a second brand and the method would
  throw when called on an instance of the class the graph actually carries.

  - **The trigger is exact rather than a guess.** `EcmaFunction` already records
    the private environment a closure resolves `#name` through, and `build_class`
    installs the class's own on every method it instantiates. So the walk asks
    whether that environment **has names**: a method that could resolve a private
    name is the kind this part carries, and a method that could not keeps the
    record it has today. That is what keeps the change off every existing class
    test, whose classes declare no private elements.
  - **The class is identified from the home object, and only when it is certain.**
    A static method's `[[HomeObject]]` is the class itself; an instance method's
    is the class's `prototype`. So the walk accepts a home object that either *is*
    a class constructor or is a class constructor's own `prototype` **by
    identity** (`C.prototype === home`), and finds the member's key by searching
    that holder's own keys for the descriptor whose member is this function.
    Anything else — an object-literal method, a class whose `prototype` was
    replaced — is not identified, and the method keeps the method record, which is
    the refusal path that already exists.
  - **Format.** `REC_CLASS_METHOD` (tag 17): the class serial, the property key as
    a slot of its own (a string or a symbol, so a computed or symbol-keyed method
    is the same record), the form (data, getter or setter, which is what tells an
    accessor's two members apart when they share one key), the home object, and
    the function's usual prototype/extensible/properties tail. The method's
    **source is not carried at all**: the class the restore evaluates already has
    the method.
  - **Restore.** The class is materialized **first**, and its build keeps the
    `prototype` object its own evaluation made — stashed by class serial and
    pinned — before the record's property tail replaces it with the carried
    prototype. The member is then read out of that prototype, or out of the class
    itself for a static, by key and form, and `make_method` re-homes it to the
    record's home object so `super` resolves through the rebuilt prototype rather
    than the evaluation's orphan. The brand is the class evaluation's, which is
    the point: instances of the rebuilt class have it.
  - **The one re-entrancy, handled explicitly.** The carried prototype's own
    properties include this very member, so the class's build reaches this record
    while the record — reached first as a value, as a `bind` target — is already
    being built. The record therefore re-reads the builder's `made` table after
    materializing the class and returns that build: the member the prototype's
    property holds, not a second method.

  *What it promises.* A method that reads a private name comes back callable on
  the instances of the class the realm rebuilt — the operation deno's
  `uncurryThis(Class.prototype.method)` performs. What it does not promise: a
  method whose class cannot be identified (an object-literal method reading a
  private name, or a class whose `prototype` was replaced) keeps today's refusal
  at restore rather than a silently wrong method.

  **Fifteenth part — a method's source is instantiated rather than evaluated,
  designed before it was written, and landed.** What the load stops on:
  `[SymbolIterator]() { return this; }` — an **object-literal** method whose
  computed key reads a binding of the module that defined it (measured:
  `home_owner=None`, so the home object is a plain object and there is no class to
  read the member out of; the home object's own property at that key *is* this
  record). The method record rebuilds by evaluating `({<source>})`, and that
  evaluation evaluates the key expression, which the reading realm cannot
  resolve.

  - **The key is not needed to instantiate a method.** A key is only what
    *installs* a method on an object; the record wants the method alone, and its
    property tail already carries the method's own `name`. So the rebuild parses
    the MethodDefinition and instantiates it from the AST —
    `instantiate_method` for `key(...) {}` and `instantiate_accessor` for
    `get`/`set`, the same two calls `build_class` makes for a class's elements.
    Nothing evaluates the key, so nothing needs the name it reads.
  - **What the parse is.** `({<source>})` parsed as a script — the object literal a
    MethodDefinition is only valid inside — with its one property taken from the
    AST rather than from an evaluated object. A source that is not one method
    definition refuses by name.
  - **The frame.** `instantiate_method` captures the method's `[[SourceText]]`
    from the enclosing frame, so the bootstrap context the instantiation runs
    under carries the parsed text as its `source` — the class record's rule, and
    what keeps `toString` and a second snapshot intact.
  - **Strictness** is the record's byte handed to the instantiation rather than a
    `"use strict";` prefix written into the source, which is also what keeps the
    method's spans exactly the parsed text's.
  - **What it does not change.** An arrow keeps the evaluation that makes it (an
    arrow *is* an expression), and a class keeps its own — so after this part
    `build_evaluated_function` serves the arrow alone.

  *What it promises.* A method whose key expression reads a name the reading realm
  does not have comes back callable, under the key the host's own object holds it
  under (which the object's record restores).

  - **The defect this uncovered, and fixed with it: an accessor's absent half was
    never readable.** `define_properties` materialized a property's `get`/`set`
    serial unconditionally, but an accessor's *absent* half is the `NO_REF`
    sentinel — a getter-only property is one with no `[[Set]]` — so the sentinel
    went to `materialize`, which answered `Truncated`. **No test had ever carried
    an object with an accessor property**, which is why the reader kept it: deno's
    blob was the first graph to reach one, once the method it used to stop on
    materialized. The reader now keeps an absent half absent, and
    `an_accessors_absent_half_stays_absent` carries a getter-only and a
    setter-only property; restoring the old code fails it with the original
    `Truncated`.

  *Acceptance tests.* Two in `crates/runtime/src/snapshot.rs`:
  `a_method_with_a_computed_key_round_trips_without_evaluating_it` (an
  object-literal method whose computed key reads a module binding, called through
  the key the object holds it under, and still answering its own source) and
  `an_accessors_absent_half_stays_absent`. Three mutations, each caught: the
  wrapper evaluated (the test fails with deno's own `SymbolIterator is not
  defined`), the evaluation's frame left without a source (the restored method
  answers the native form), and `make_method` dropped (the method part's own
  `super` test fails); the accessor defect has its own mutation above.

  **And the load is past the engine.** With this part deno's blob reads through
  every value the walk carries, and deno_core's own extraction runs for the first
  time — where it stops on something that is **not** the engine's: it attached
  **five** items to its bootstrapped context and reads them back by index, while
  the blob carries **two**. Measured: three of the five are `Payload::Module` (a
  module record, which is not a language value), and the bridge's `items_of`
  **drops** every item its `engine_value` cannot turn into a `Value` — silently,
  so the host's indices shift rather than fail. That is a bridge defect and a
  design decision (carry a module record by specifier, or refuse loudly), and it
  is part 16.

  **Sixteenth part — a module record is an item a slot carries, designed before
  it was written, and landed.** The first part of this item whose defect was the
  bridge's rather than the engine's, and it is fixed where the host can act.

  - **What the write side measured.** Deno attached **five** items to its
    bootstrapped context, three of them `Payload::Module` (the ES module records
    its module map serializes); `items_of` mapped each through `engine_value`,
    which turns a `Data` into a `Value` or nothing, so `filter_map` dropped the
    modules and the host's numbering shifted — its index 2 answered the item that
    belonged to a later index. Nothing failed at the engine's boundary.
  - **The decision: carry the record.** Refusing the attachment loudly would
    cost deno its whole snapshot to report a value the engine can rebuild. The
    item carries the module's **name** and **source**, because that is all a
    record keeps: the specifier is the host's (`Module::register` is what writes
    one into the realm), and a host re-registers a restored module under its own
    — which is what deno's `update_with_snapshotted_data` does. Link status,
    bindings and namespace are not carried and are not claimed; the restored
    record is fresh and unlinked.
  - **Format and restore.** `REC_MODULE` (tag 18): a presence byte and the name's
    units, then the source's units. `SnapshotItem` is `Value | Module`, so a
    slot's items are an **item** list rather than a value list; the two paths that
    can only answer with a value — `decode`'s single-value form and
    `Builder::materialize`, which a record *field* uses — refuse a module, because
    only a slot's own item list can name one. The restore compiles through
    `parse_module` under a bootstrap context it pushes and pops on every path (the
    engine compiles in the agent's **current** realm) and pins the record with
    `pin_handle`, a module being no language value a handle scope could hold.
  - **No new public surface.** `SnapshotItem::Module` names `api::Module`, so the
    bridge hands over what its `Payload::Module` already holds and takes back what
    the engine returns; the engine grew one `pub(crate) fn handle`, for the
    identity the format keys a module by.
  - **The bridge's filter is gone, and the refusal moved to the attach.**
    `add_context_data` decides the item, so a handle that is neither a value nor a
    module is refused **by kind and position** (`Payload::kind`, new — "a
    context", "a script", "a stack frame"; a `Context` is a `Data` the crate's
    own tag checks allow) at the call the host can act on. `Attached
    { item, held }` keeps the handle for its pin, which is also what makes
    `items_of` total without a second refusal path.

  *Acceptance tests.* Three, all landed:
  `snapshot::tests::a_module_is_an_item_like_any_other` (a module at index 0 and a
  number at index 1 of one slot, the module's source asserted and the number still
  at index 1), the bridge's `an_attached_module_round_trips_at_its_own_index`
  (deno's shape, each index read back once) and
  `a_context_attached_as_context_data_is_refused` (the refusal by kind). Four
  mutations, each caught: `materialize_item` refusing a module (fails with
  `Truncated`), that arm answering `undefined` (fails on "the module came back as a
  language value"), `items_of` filtering modules out again — which fails **exactly
  as deno's load did** — and `Payload::kind`'s context arm answering "a value".

  **And the load now stops on the framework's own assumption rather than on the
  engine.** With this part deno_core's extraction runs to the end — all five
  indices read — and the load dies in `initialize_deno_core_ops_bindings`
  (`jsruntime.rs:1087`) on a missing `globalThis.Deno`: the state
  `InitMode::FromSnapshot` expects **V8 to have deserialized**, since
  `jsruntime.rs:1080-1083` skip `initialize_deno_core_namespace` and
  `initialize_primordials_and_infra`, `1264-1279` skip the virtual ops module and
  the builtin sources, and `1282` reads `Deno` unconditionally as well. No further
  item carries it: deno's five attached items are its module map, its templates
  and two handles, not the global object. The **from-source** path is the one this
  engine serves, and this run shows it — the example's build phase
  `create_snapshot` with `startup_snapshot: None`, so `InitMode::New`) runs the
  whole bootstrap on this engine and writes its blob. That is a decision rather
  than a record, and it is §12's eleventh item; §7's last record has the anchors.

  **And a host's frozen namespace exposed a reader defect, fixed inside the
  measurement.** The carried-global experiment §12's eleventh item called for
  ended by finding a real bug rather than a design wall: deno **freezes**
  `Deno.core` and `Deno.core.ops`, and the reader applied a record's `extensible`
  flag **before** defining the record's properties, so
  `[[DefineOwnProperty]]` refused every one of them (`refused 147 of 147`, then
  `832 of 832`) and `define_properties` discarded the refusal — the objects came
  back as shells. The flag is now applied **after** the property list at all six
  reader arms that set it (`Record::Object`, `Record::Array`, and the four
  function-shaped ones), one test covers a frozen object, a frozen array and a
  non-extensible function, and three mutations (one per arm shape) are each
  caught. **This is the deviation worth recording:** §11 asks for `runtime` edits
  to be named *before* they are made, and this one was made inside a measurement.
  §7's record states it too, so it is visible in both places rather than smoothed
  over.

  *Acceptance tests.* Three, all landed:
  `snapshot::tests::a_method_that_reads_a_private_name_round_trips_as_its_classs_member`
  (the measured deno shape — a public method calling a private one — carried as a
  class member and called on an instance of the restored class, so the private
  brand has to match, with the blob asserted to spell the record),
  `snapshot::tests::a_private_name_method_reaches_super_through_the_carried_prototype`
  (the re-homing: the mutation is on the carried prototype, which only the
  carried object has), and the bridge's
  `a_private_name_method_round_trips_and_reads_its_classs_private_field`. Three
  mutations, each caught: the trigger disabled (the blob carries no class-member
  record), the re-homing dropped (`super.added` is `undefined`), and the
  constructor guard removed — which the existing private-field test turns into a
  stack overflow, because a class constructor's own `constructor` property names
  the class as a member of itself.

  **And the load moves past it.** With this part deno's blob reads further than it
  ever has: the private-name method is carried, and the restore stops on the
  **class** — `its class source could not be evaluated: ReferenceError:
  "SymbolIterator" is not defined`. That is part 7's stated divergence arriving as
  a hard failure: a class's definition-time code runs again at restore, so a
  **computed key** (or a static initializer) that reads a name from the module
  which defined the class finds the reading realm's global instead. The engine
  already has the mechanism that removes the re-evaluation —
  `class_definition_evaluation_with_scope` takes the element keys
  **precomputed** — so the next part is to carry them. That is part 14.

  **Fourteenth part — a class's definition-time inputs are carried with it,
  designed before it was written, and landed.** The class record rebuilds a
  class by
  evaluating its source, which is part 7's design and stays: what a class *is*
  (its `[[ConstructorKind]]`, fields, private methods and environment, prototype
  object, and the constructor's body) is what only the engine's own class
  evaluation makes. Part 7 therefore recorded that the definition-time code runs
  again — computed keys, `static {}` blocks, static field initializers — as a
  stated divergence. The load shows the divergence is not benign: a **computed
  key** that reads a name from the module which defined the class
  (`[SymbolIterator]() { return this; }`) cannot be evaluated at all in the
  reading realm, whose global has no such name, and the same is true of an
  `extends` expression naming one.

  - **What is carried is the *input*, not a re-derivation.** `build_class`
    already resolves every computed public element's key through
    `element_key_with`, and indexes them exactly as `precomputed_keys` does (one
    per computed public element, in source order). The class's constructor
    record gains `computed_keys: Vec<PropertyKey>` — those keys, as the engine
    resolved them at definition — and the class record writes them. The heritage
    goes with them: `resolve_heritage` derives the proto parent from the heritage
    *value*'s `prototype`, so carrying that value reproduces the class exactly,
    and `extends null` is carried as the value `null` — which the writer can tell
    apart, because a null heritage is the one case whose super constructor is
    `%Function.prototype%` with no proto parent.
  - **Format.** The class grammar's record gains, in the grammar-conditional slot
    a method uses for its `home`, a **heritage** serial (`NO_REF` for a class with
    no `extends` clause) and a count-prefixed list of **key** serials. Both are
    ordinary value slots, so a symbol key is written as the symbol it is.
  - **Restore.** The source is parsed as `(<class source>)` — the expression form
    a class source is only valid as — the class expression taken from it, and the
    evaluation handed to `class_definition_evaluation_with_keys`, the engine's own
    entry for a class whose heritage and keys were evaluated elsewhere. Nothing is
    re-evaluated. The parse is a script, so the class's spans index the text the
    parse was given, and the bootstrap context the restore pushes carries that
    text as its `source` — which is what keeps `capture_source` able to answer the
    class's `[[SourceText]]`, so `toString` and a second snapshot are unaffected.
  - **What this still does not carry:** a `static {}` block and a static field
    initializer *execute* at restore, because they are effects rather than inputs.
    That stays the stated divergence it is; part 7 records it.

  *What it promises.* A class whose definition-time code reads a name from the
  module that defined it comes back defined, with the same keys and the same
  heritage — so a carried method, a carried field's key, and a carried subclass
  all resolve to what they did before.

  *Acceptance tests.* Four in `crates/runtime/src/snapshot.rs`:
  `a_class_carries_its_computed_field_key` (a computed field key reading a module
  binding, and the class still answering its class source),
  `a_class_carries_its_heritage` (the implicit constructor calls `super()`, which
  only the heritage decides — stated in the test because the statics would
  survive without it, carried by the class's own prototype link),
  `a_class_carrying_a_null_heritage_stays_derived` (the one heritage whose
  resolved form is not its written form), and a
  `a_class_carries_a_computed_method_key` (both halves at once: the class carries
  the key, and the method is a class member). Three mutations, each caught: the
  keys not written (both key tests fail with deno's own `SymbolIterator is not
  defined`), the heritage always `None` (both heritage tests fail — the class
  comes back a base class), and the evaluation's frame left without a source
  (the restored class answers the native form instead of its class source).

  **And the load moves past the class.** With this part deno's blob reads past
  the class it stopped on and stops on an **object-literal method with a computed
  key** instead — `[SymbolIterator]() { return this; }`, whose home object is a
  plain object (`home_owner=None`, measured), so there is no class for a
  class-member record to belong to and the method record's own source is what
  re-evaluates the key. The same mechanism does not reach it: what that value
  needs is the method *parsed and instantiated* rather than evaluated, so the key
  expression is never evaluated at all. That is part 15.

- **A callback scope's handles carry the lifetime of what the scope was opened
  from, not the borrow of its storage.** That is the crate we stand in for's own
  choice, and this bridge reproduces it because a host's helper has to be able to
  return a handle it made in a callback scope (two `deno_core` sites do, §7). The
  obligation it places on a host is real, and what it costs here is a panic
  rather than undefined behavior: the one payload this bridge ties to a region is
  a script's source text, so a script handle that outlives its scope reads a
  released slot and panics, where the crate's pointer would simply dangle.
- **The bridge always compiles the engine's WebAssembly, and a host pays for that
  in its lockfile rather than in its source.** The crate we stand in for has
  WebAssembly unconditionally — `v8::WasmModuleObject` and the tags beside it
  exist whether or not a host names them — so `crates/v8` asks `slag` for its
  `wasm` feature, and the engine's wasm is in every host's graph. On native
  targets that feature also carries the engine's Cranelift codegen (the
  `cfg(not(target_arch = "wasm32"))` table in `crates/runtime/Cargo.toml`), so the
  host's lock must be able to admit Cranelift 0.134: `deno/`'s could not until six
  leaf crates were bumped (§7, §12 item 10). A host's source needs no change at
  all; the alternative engine design — a feature meaning "the JS API without the
  native codegen" — is recorded as open rather than half-built, because the cheap
  form of it is refused by cargo.
- **A compiled wasm module here is the decoded module, and its wire bytes are
  deliberately absent.** V8's `CompiledWasmModule` is a shared allocation that can
  hand out the bytes it was compiled from; this engine decodes into structures and
  keeps none, so `get_wire_bytes_ref` and `source_url` have no answer and are
  missing rather than wrong. Retaining the bytes would cost a copy per `Module`
  object through every wasm sweep, and nothing in the frontier asks for them.
- **`WebAssembly.compileStreaming` exists only when the host installed a streaming
  callback, and says so with a `TypeError`.** V8 requires an embedder hook for the
  method and `DCHECK`s that one is there
  (`v8/src/wasm/wasm-js.cc:889`), which in a release build is a crash inside the
  engine and in this one is an exception a host can read: an isolate whose host
  installed neither `set_wasm_streaming_callback` nor
  `set_promise_reject_callback` keeps the engine's default hook, and
  `has_wasm_streaming_callback` answering `false` is what refuses the call. The
  other half of the divergence is inside the same method: bytes accumulate and are
  decoded at `WasmStreaming::finish`, not as they arrive. A host sees the same
  module and the same promise — the compile-as-it-streams is what is missing, and
  it is not observable through the API.
- **The code cache a host stores is the code's own source text, and it is inert.**
  V8's `create_code_cache` answers serialized bytecode; this engine keeps no
  compiled form, so there is nothing to serialize, and `None` — the crate's
  documented answer for that — would break `deno_core`'s default configuration
  rather than report the gap (its module map asks on every load and treats the
  empty answer as a failed load; see §7's record, and the ledger's item 11 for
  the decision). So the bytes are the source text, which a host stores and hands
  back and the engine ignores, and `CachedData::rejected()` answers `true` always
  so that a host which trusts the flag re-produces its cache. The cost, stated:
  the host's cache database holds a copy of the source and the round trip saves
  nothing. Nothing here is ever believed that was not checked, which is the
  property that makes the divergence inert rather than wrong.
- **A source map URL is read from the source text, not from a lexer.** V8's
  scanner recognizes the magic comment as syntax (`scanner.cc:280`) and keeps the
  last one it sees, wherever in the source it is. The bridge's `UnboundScript` /
  `UnboundModuleScript::get_source_mapping_url` reads the source as text and
  accepts only a comment that occupies a line of its own — the shape every
  emitter writes, and the one a string or template literal containing the same
  characters cannot plausibly have. Both directions of the difference are real:
  a mid-line magic comment that V8 would find answers `undefined` here, and a
  whole-line one that V8's *scanner* would reject as string content would be read
  as a comment. The first is the cost of having no lexer; the second is why the
  rule is line-anchored rather than a plain search.
- **A stack trace reports one frame per call, except for a leaf-inlined one.**
  The engine pushes an execution context for every ordinary call, so a trace
  reports the activations it runs: a script, a module, an eval, an async or
  generator resumption, and each call. A **leaf-inlined** call is the one
  omission — a leaf body is run in place on its caller's Vm by design, and
  leafhood excludes exactly what would need a frame (closure creation, calls, a
  sloppy `arguments`) — and JavaScript cannot observe the omission, because
  reaching a trace means making a call. What a call's frame does not carry is a
  **name**: the certified push fills `context.function` only for the one reader
  certification leaves in it, so `GetFunctionName` answers nothing for a call
  that does not read one, and `Error.stack` — which names its lines from the same
  slot — reads as it did. V8 would answer the name; that is a value store on the
  call path and is named in §12 rather than taken.
- **Three frame flags answer `false` and one answers `true` for every frame.**
  `is_eval`, `is_constructor` and `is_wasm` are `false` because an execution
  context does not record them — eval code even inherits its caller's script or
  module, so nothing separates the two — and the activation's `new.target` is not
  kept. `is_user_javascript` is `true` because the engine's Rust builtins never
  push a context and it classifies no script as native, so every frame a host can
  see really is running JavaScript. Each is stated in the accessor's own
  documentation as well, because a host reads the flag and acts on it.
- **A frame's line and column are V8's "no information" values.**
  `Message::kNoLineNumberInfo` and `kNoColumnInfo` are both 0, and both are 0 for
  every frame here: the engine records a position where an *error* is made, not
  where a frame is running. That is the same gap `Message`'s runtime location has,
  and the plan's survey already split it off from this work as the per-activation
  source positions (§10).
- **A module's `name` is the host's name for it, and a classic script has none.**
  A stack frame reports the name of the code it runs, and the only place a name
  can live is the record: `SourceTextModule::name` is set when the module is
  parsed (from a parameter `api::Module::compile_with_name` threads, or the
  specifier a host-provided source was resolved under), while a script parsed by
  `parse_script` carries none — threading one would touch the eval path every
  runner uses, and no site in the frontier needs it.
- **`HeapStatistics` reports the engine's own numbers, four of the crate's
  fifteen, and the definitions are not V8's.** The heap here is a chunked bump
  arena: `total_heap_size` and `total_physical_size` are the same number (the
  committed bytes — an arena that never over-reserves has one number for both),
  `used_heap_size` is the live boxes' footprints as the arena walk counts them,
  and `external_memory` is the bytes of the buffers the agent holds, summed over
  its own record of live buffer objects rather than measured. The other eleven
  accessors — a heap limit, an allocation total, malloced memory, handle-registry
  sizes, executable bytes, the zap flag — have no honest value here and are
  *absent*: a host that names one gets a compile error rather than a plausible
  zero. A host comparing its numbers to V8's must read them as "what this engine
  holds", not as the same quantities.
- **A deferred import's promise settles with the deferred namespace, where V8's
  settles with the result of an internal `Promise.all`.**
  `Module::EvaluateForImportDefer` answers a promise mostly for its *state*: the
  one caller in the frontier branches on it to tell a module with no
  asynchronous dependency — which V8 resolves before returning — from one still
  waiting, and resolves its own promise with the deferred namespace either way
  (`libs/core/modules/map/dynamic.rs:621-633`). What the promise *carries* in the
  waiting case is an implementation detail of V8 that nothing reads; here it is
  the deferred namespace, because that is what the engine's existing wait
  settles with. Stated so that a host inspecting it knows it is reading a
  different value.
- **A stalled-await message has no location.** V8 mints a `JSMessageObject`
  carrying `kTopLevelAwaitStalled` and a `MessageLocation` for the module's
  script and the generator's resume offset
  (`v8/src/objects/source-text-module.cc:1620`), so `GetScriptResourceName` and
  `GetLineNumber` answer on it. The engine records no position for a suspended
  module, so both answers here are the ones this bridge gives any message with no
  recorded position — none — and the text is the template's own. The frontier's
  one consumer reads the text and treats a missing location as a message with no
  frame (`libs/core/error.rs:841`), so the report a user sees is V8's wording;
  the source line V8 prints underneath it is what is missing.
- **A synthetic module asked for stalled awaits answers nothing, where V8
  aborts.** `GetStalledTopLevelAwaitMessages` `ApiCheck`s that its receiver is a
  source text module (`v8/src/api/api.cc:2622`) — a crash in a release build —
  and a synthetic module's body is a host callback rather than a source graph, so
  the walk here answers the empty list instead. `deno_core` asks only behind its
  own `is_synthetic_module` guard (`evaluation.rs:343`), so the divergence is
  unreachable from the frontier's host and recorded for the next one.

## 10. Build order

Engine side: (1) L1 roots — done; (2) platform + task runner; (3) snapshot +
external references + per-isolate/context data slots — **the format landed with
its context table, the external-reference table, a function, a bound function, a
class constructor, a host callback, an arrow, a method with its `[[HomeObject]]`,
the realm's named global functions, the members of its own builtin objects, a
method that reads a private name, a class's own definition-time inputs and a
method instantiated from its parsed definition, and **a module record as the
name and source the engine's own compile path rebuilds it from** with it (§7's
records for it;
ledger item 16 has sixteen parts). What is left of
this item is named rather than implied: `FunctionCodeHandling::Keep`'s compiled
code (which waits on the code cache), the isolate-level data slots, continuation
from an existing blob, and what the **load** now stops on — which is the
framework's own assumption rather than the engine's: deno's `InitMode::FromSnapshot`
path skips its bootstrap because it expects a **deserialized heap**, and reads
`globalThis.Deno` (`jsruntime.rs:1080-1083` skip the namespace and primordials,
`1282` reads `Deno` unconditionally, and `1087` is where the load now dies),
while deno's five attached items are its module map, its templates and two
handles rather than the global. No further item carries that state, so a data-only
blob cannot stand in for a heap image on that path — and the **from-source** path
this engine does serve is one the example's own build phase already runs to the
end (§7's last record; §12's eleventh item is the decision) —
behind which is still the measured question of the graph reaching
the **global object**, and behind that the accessor half of the method part
(**landed** with part 15, §7).**;
(4) module resolver as a
host trait (landed), **unbound scripts and script origins** (the bridge side
landed — every Rust script handle is already context-unbound — and what the
engine still owes there is the *code cache*: serializing its compiled program, so
that the bytes a host stores are worth storing; §7's record and the ledger's item
11 state why the bridge hands out the source text meanwhile). Then, in the order
the
shim histogram implies: **structured frames and termination** — the frame
accessor landed (§7, and the ledger's item 12), while what a *stack trace* still
needs is the VM's own frames: the engine's execution contexts are not a call
stack (measured in §7), so a per-call view means recording activations in the
interpreter and the JIT, and the per-activation source positions that every
`StackFrame` line number needs went with it; the compile-time half of a message's
position landed as bridge work (`crates/v8/position.rs`, §7), and what is left
for the engine is the *per-activation* half a runtime error would need — then host
memory (partly
landed — a store over memory the host owns is `SharedBuffer::borrowed`; the
accounting half, externally allocated memory and backing-store shrink, is not),
synthetic modules (host-filled records with a host evaluation callback, §12 item
9 — **landed**; the engine's JSON/text/bytes shortcut is why it had none until
then), the wasm tail (§12 item 10 — **both slices landed**: the compiled module
was an exposure, streaming was a new isolate hook), inspector and
source maps, traced host objects, structured clone.

Bridge side: (1) signature-compatible Rust face — done, `serde_v8` type-checks;
(2) grow the surface from the items `deno_core` names, in call order — the name
frontier is closed (the last two, `WasmStreaming`, resolved with the streaming
half) and the serializer is
landed, so this
is a **method-level** stage: 617 type errors were visible for the first time, the
first pass through them took it to 374, the cast closure to 369, the scope and
property bounds to 364, the identity hashes to 351, the embedder-data slots to
347, private names to 336, the method tail to 292, scheduling and exception
control to 260, the isolate-level callback vocabulary to 242, the symbol surface
to 233, the primitive array to 225, the leftovers to 217, the buffer-handing
shapes to 213, the host-memory store to 209, the tag shape to 62, the template
surface to 56, the attribute-carrying half of the template cluster to 52, the static
half of it to 49, the stragglers a `Context` and an `Object` answer to 44, the
`Message` surface and `Exception::create_message` (piece 2 of the stack-trace
split) to 37, the module structure surface to 25, the synthetic-module surface to
21, the callback scope's lifetime to 19, the compiled-module surface to 15, the
streaming half to 12, the unbound scripts and the code cache to 6, the
stack-trace frames to 4, the escapable handle scope to 3, the heap statistics to
2, and the module-graph tail to **0** — the crate type-checks against this one,
with no stragglers left at all. That metric is spent: what is left of (2) is no
longer type errors but the runtime gaps a host would hit the moment one ran, and
(3) is where they surface.
The two that the suite had surveyed as needing engine work have landed since,
`get_constructor_name` and `get_extras_binding_object`, and they turned out to
need a walk of the prototype chain and a per-context object rather than V8's map
and an engine-side extras object. Everything else needs the tag shape or an
engine capability; with the shape
landed, the tag-shape column is closed — what remains is methods to write and the
subsystems they name; and the shape's own tail is (a) the methods moving
from `LocalHandle` onto the tags, file by file, then (b) deleting `LocalHandle`
and its deref table, which is when the tier §9 states stops being a tier;
(3) point the local `deno/` checkout at the crate and run a script — the
`deno_core` type errors are gone, `deno_core`'s own bootstrap now *runs* (§7's
last records: both snapshot build scripts build a `JsRuntimeForSnapshot`), and
what stopped them was the engine-side item (3) below rather than anything in the
bridge. That item's sixteen parts have landed — the format, the context table,
the external-reference table, a function, a bound function, the realm's named
global functions, a class constructor, a host callback, an arrow, the text a call
frame runs, **a method with its `[[HomeObject]]`**, **the members of the
realm's own builtin objects by name**, **a method that reads a private name,
as a member of its class**, **a class's own definition-time inputs**, **a
method instantiated from its parsed definition** and **a module record, as the
name and source the engine's compile path rebuilds it from** — so the blocker
this plan
could name is
gone: the
format carries a slot per context, `deno_core`'s data lives on the realm it
added at slot 1, a host pointer is an index into the table the host rebuilds
for every load, a function, a bind, a class, an arrow, a host callback, a method,
a builtin member, a private-name method, a class whose keys and heritage come
from the module that defined it, a method whose computed key does all
come back
callable or
constructable, a closure deno's own JS creates inside a function the host
calls from Rust carries its text, and every item a host attached to a context
comes back at the index it attached it under. **`create_blob` completes** —
deno's
snapshot build script runs to the end and writes its blob — and the
frontier is now **deno_core's own start-up assumption**: the extraction that
used to stop on the dropped module records runs to the end (all five indices),
and the load dies in `initialize_deno_core_ops_bindings` (`jsruntime.rs:1087`)
on a missing `globalThis.Deno`, which `InitMode::FromSnapshot` expects a
**deserialized heap** to provide (`1080-1083`, `1264-1279`; `1282` reads `Deno`
unconditionally). No item deno attaches carries that state — its five are its
module map, its templates and two handles — but §7's last record measured the
state itself: a bootstrapped realm's `globalThis` **walks** (2,833 records /
220,054 bytes, no refusal), a **fresh isolate restores it** with the writer's
`Deno.core` (147 keys) and its bootstrap functions **run**. So the frontier is
now the **integration** — the load applying a carried global to the realm it
rebuilds, before the host reads its own names — with the from-source path
(`InitMode::New`, `startup_snapshot: None`, which the example's own build phase
already runs to the end) as the other measured answer. §12's eleventh item
names both and is where the choice is recorded. The `ext`-crate frontier of §7's
measurement
(341 errors across eight crates, none of them `deno_core`) is still what stands
between this and a `deno` binary, and
deno's own runtime gaps (`queueMicrotask` among them) sit behind that; (4)
migrate, then delete.

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
2. **Snapshot format** — **v1 landed with its context table, external
   references, a function, a bound function, a class constructor, a host callback,
   an arrow, the realm's
   named global functions,
   the text a call frame runs,
   a method with its [[HomeObject]], the members of the realm's own builtin
   objects, a method that reads a private name, carried as a member of its
   class, a class's own definition-time inputs, a method instantiated from
   its parsed definition and a module record as the name and source the engine's
   compile path rebuilds it from** (§7's records for it, ledger
   item
   16's sixteen parts): a versioned
   blob over a
   value graph rooted at the data a host attached to each of its contexts,
   written and read against each slot's own realm, intrinsics by name, host
   pointers as indices into the host's table, a JavaScript function as the source
   it is re-parsed from, a bind as its target and bound state, a class
   constructor and an arrow as the expressions the engine's own evaluation
   re-runs, a host
   callback as the entry of the host's table its call came from **and the data
   value it reads**, a module record as the **name and source** the engine's own
   compile path rebuilds it from — so a slot's items are an item list rather than
   a value list, and every item comes back at the index it was attached under — a
   refusal naming
   anything uncarried. And the format now **loads**: deno's blob, read back for
   the first time, restores contexts past every value it used to stop on — the
   realm's own builtin members by name, a method that reads a private name, a
   class whose computed keys and heritage come from the module that defined it, a
   method whose computed key reads one, and the module records it attached — so
   deno_core's own extraction runs to the end, and what the load then stops on is
   the framework's start-up assumption rather than this format's: an
   `InitMode::FromSnapshot` runtime expects a **deserialized heap** to carry
   `globalThis.Deno` (`jsruntime.rs:1080-1083`, `1264-1279`; read at `1087` and
   `1282`), which no attached item is. That is item 11 below. What this item
   still owns: a host callback's
   **construct half**, the
   **accessor**
   half of the method part (**landed** with part 15), the
   measured question of the walk reaching the **global object**, a
   value shared between two contexts coming back one per context, compiled code
   for `FunctionCodeHandling::Keep`, isolate-level data, and continuation from an
   existing blob. The realm's own builtin members by name — the value the walk
   met as `Math.abs` — are **carried** (the twelfth part), so is a method that
   reads a private name (the thirteenth), so are a class's computed keys and
   its heritage (the fourteenth), and so is a method whose own source's key
   expression cannot be evaluated (the fifteenth). The call-frame
   `source` the eleventh part closed was the conformance bug this item's walk
   exposed rather than a record it was missing.
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
7. **The bridge's position table has no way to forget an entry.**
   `IsolateInner::positions` (and `runtime`'s own `error_data` / `error_stack`)
   is keyed by object identity, which only a collection can retire, and the
   sweep hook that would prune it (`Agent::compact_weak_tables`, which sees the
   dead addresses) is the engine's. It grows by one entry per *positioned* error —
   a host in a watch loop that recompiles a broken file is the case to think
   about — and the values are the record and the script name, so the shape is the
   engine's own existing tables's. Either the engine grows a hook a bridge can
   register, or the table is rebuilt per compile, and both are decisions rather
   than drive-bys.
8. **A host helper that ties a handle to the callback scope's own lifetime —
   landed** (§7). Two sites, one shape, one fix: a callback scope's `Deref` now
   hands out handles at the scope's own parameter — the context's lifetime, which
   its constructor binds — rather than at the borrow of its storage, and a
   `tc_scope!` opened over a callback scope follows it (its `NewTryCatch` now
   threads that lifetime rather than the borrow). `E0515` went 2 → 0, and the
   guard is a test that settles a promise under both scopes and returns it at the
   caller's lifetime.
9. **Synthetic modules — landed** (the engine half and the bridge half; §7
   records both, and §9's bullet records the two shape decisions). What the
   engine grew: the `SyntheticModule` aspect on `SourceTextModule`, the four
   `api::Module` methods, and the four dispatch points (instantiation, the
   namespace's names, a read of an export, and the steps themselves, with the
   promise kept so a second evaluation does not re-run them). What is *not*
   there: duplicate export names are not refused (V8 throws for them, and the
   crate's own `.unwrap()` panics), because the engine's `create` cannot fail and
   a host that passes one gets a record whose cells are ambiguous rather than an
   error — a named follow-up, not a guess.
10. **A call's frame carries no name — open, and its cost is measured.** The
   certified push fills `context.function` only for the one reader certification
   leaves in it (a sloppy body's mapped `arguments`), so every other call's frame
   answers `None` to `v8::StackFrame::GetFunctionName` (and `Error.stack` names
   none of them). Filling it unconditionally is one `Value` store per certified
   call — comparable to the `parse_text` clone the eleventh part of item 16 added
   to the same push — and it *changes* `Error.stack`'s text, which is why it is a
   decision rather than a line: the engine's stack strings are what deno's CLI
   prints, so the change wants its own measurement rather than a ride on this
   one. §9's stack-trace bullet states the divergence meanwhile.
11. **What a host's `startup_snapshot` means when the engine has no heap image —
   open, measured, and the operator's call.** With the sixteenth part of item 16
   deno_core's own extraction runs to the end and its load then dies in
   `initialize_deno_core_ops_bindings` (`libs/core/runtime/jsruntime.rs:1087`) on
   a missing `globalThis.Deno` — the state `InitMode::FromSnapshot` expects **V8
   to have deserialized**: `1080-1083` skip `initialize_deno_core_namespace` and
   `initialize_primordials_and_infra`, `1264-1279` skip the virtual ops module
   and the builtin sources, and `1282` reads `Deno` unconditionally as well. The
   engine's blob is a **value graph rooted at the data a host attached**, and
   deno's five attached items are its module map (three module records in this
   graph, §7), its function-template data and two handles — not the global
   object, so no further item carries what is missing. Two paths exist and the
   choice is silent no longer:

   - **Boot from source (`InitMode::New`, `startup_snapshot: None`)** — the host's
     own bootstrap runs on this engine and the data-only blob is not passed at
     all. This is the path the engine already serves end to end: the example's
     build phase *is* it (`create_snapshot` with `startup_snapshot: None`), and it
     binds ops, evaluates the builtin sources, instantiates the extension's
     `esm_entry_point` module and writes its blob. A host's own source needs no
     change beyond not passing a snapshot it cannot use; what it gives up is V8's
     heap-image startup, which is the whole point of a snapshot.
   - **Carry the realm's global object** as one more root — **measured end to end,
     and it works.** §7 records the experiment in full, with the numbers and the
     defect it had to fix. Its outcome: the walk carries a bootstrapped realm's
     `globalThis` with **no refusal** (slot 1: 2,833 records / 220,054 bytes); a
     **fresh isolate** restores it with the writer's `Deno.core` (147 own keys,
     the same names) and 72 of 72 properties applied; and a restored *bootstrap*
     function **runs** — `Deno.core.setUpAsyncStub` reaches deno's own argument
     check rather than a `ReferenceError`, because deno's bootstrap reads its
     state through `globalThis.__bootstrap`, which a carried global contains.
     What that rules out is the earlier version of this bullet ("the walk refuses
     the first proxy, `WeakMap` or typed array") **and** the inference that
     followed it (a restored bootstrap function would resolve its module bindings
     as globals and fail); both were reasoned about rather than measured, and §7
     is where the measurement is.

     What is **not** done is the integration, and it is the part this item now
     names: the load path has to *apply* a carried global to the realm it is
     restoring into, before the host reads its own names off it. That is a bridge
     change (`SnapshotRestore::restore` has the fresh realm and the decoded
     items; deno's `create_context` has already made the context) plus whatever
     the engine needs to make "this realm's global state is one of its slot's
     items" a supported shape rather than a host's private convention — and the
     measurement says nothing about *which* of the two a host should prefer, only
     that both are reachable.

   What is *not* decided here: whether a Slag-backed host ships a snapshot at all
   (the from-source path needs no snapshot, and the carried-global path needs
   one), and whether the engine should make the distinction visible (an
   `is_valid`-answering blob a host can check before trusting it, or a
   `slag`-level "data-only" snapshot a host opts into), which is a bridge and API
   question rather than this format's. Recorded so "deno on Slag" stops meaning
   "deno's prebuilt heap snapshot on Slag", which the measurement above rules out,
   and starts meaning one of two measured paths: deno's from-source boot, or a
   boot whose realm's global state came out of the blob.
12. **`Object.assign` and a function — landed, both roles fixed.** §7's
   measurement found it in passing: `Object.assign(function () {}, { tag: 7 })`
   throws `TypeError: assign target is not an object`, because `object_assign`
   (`crates/runtime/src/builtins/object.rs:1506-1523`) matches `ValueKind::Object`
   for the target while `to_object` answers `Value::Function` for a function, and
   the same match **skips a function source** entirely — so a function's own
   enumerable properties are never copied either. A function is an object in both
   roles (spec 20.1.2.1 coerces the target with ToObject, and a source is any
   coercible value), so it was a conformance divergence with no fixture over it.
   The fix was the engine's own coercion (`crate::context::as_object`, which the
   snapshot reader and the builtins already use where a function must be treated
   as an object) in the two matches, with a test for each role and a sweep of a
   corpus that has `Object.assign` in it.

   **Landed** (named before the edit, §11's order). Both matches go through the
   engine's coercion, and the source's properties are read with the coerced
   source as the receiver, so a getter sees the function rather than its object
   part. `builtins::object::tests::assign_treats_a_function_as_an_object` covers
   both roles and the enumerable-only rule (a function source's non-enumerable
   `name` does not travel); two mutations, each caught — the target coercion and
   the source coercion reverted to the `ValueKind::Object` match. Part 17 of §7's
   record; the corpus was re-swept because `Object.assign` is in it.
13. **An accessor had no `[[SourceText]]` — landed, and it was a conformance
   gap rather than only a snapshot one.** Found while fixing the
   first: a getter created by an object literal in an eval'd script refused to
   encode (`a function — the engine kept no source text for it`). The reason was
   in the instantiation: `instantiate_method` (`function.rs:1023`) called
   `capture_source(agent, f.span)` and handed it to `register_function`, while
   `instantiate_accessor` (`function.rs:1051`) took `params` and `body` with **no
   span at all** and registered the function with `source: None`. §7's part-8
   measurement already named the shape ("the same context is what an accessor
   needs"), and §7's part-11 record re-aimed the refusal test onto the accessor as
   the kind that still had no `[[SourceText]]` at all — so this was a documented
   gap being closed, not a new discovery.

   It mattered beyond the snapshot: spec 15.4.3 gives a getter or setter defined
   by a `MethodDefinition` the definition's text, so its
   `Function.prototype.toString` must answer `get b() { return 2; }` — and the
   corpus cannot see the difference, because test262's
   `assertToStringOrNativeFunction` accepts the native branch (the same reason the
   class-constructor and arrow gaps before it were invisible to it). **Landed:**
   the accessor instantiation takes the definition's span and captures the source
   like the method path, and every caller passes the span the parser now records
   for the whole definition — `class.rs`, `expr.rs` and `ir.rs` (the span rides on
   the two `ObjectAccessor*` steps, and the JIT passes the step index to its
   helper, so `crates/jit` is unchanged) — while the snapshot's rebuild hands over
   the record's own text. A getter or setter is carriable by the same method record
   an accessor's half already uses, and the engine's source-less refusal test had
   nothing left to refuse by kind, so it became positive coverage
   (`an_accessor_round_trips_with_its_source`). Six mutations, each caught; the
   corpus was re-swept because `toString` is in it. Part 17 of §7's record.
