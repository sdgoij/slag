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
| **L2** | own objects JS retains | traced host objects (host references participate in marking) + finalization + weak handles | **edges and finalization landed**; weak persistent handles remain |
| **L3** | own scheduling, GC coordination, threads | platform/task runner, microtask policy, snapshots, external references, termination, host memory | **partly landed** — `MicrotasksPolicy` only |

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
   once, after the collector decides it is unreachable. **Landed**.
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

Stale text this leaves behind: `crates/runtime/src/api/handle.rs` still describes
`HandleScope` as "Advisory under `Rc`", and §5's paragraph on why L1 is cheapest
leans on the same wrong model when it says a scope needs "rooting but no update
machinery". The conclusion survives (the arena does not move objects), the reason
does not.

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

**Next: signature compatibility.** The real crate's handles are generic —
`Local<'s, T>`, `MaybeLocal<T>`, `Global<T>`, `HandleScope<'s, C>` — while the
engine's are a single non-generic `Local`. That is what stops `deno_core` from
type-checking, and it is done *inside* `crates/v8`, wrapping the engine's
values, so `runtime` stays untouched. Gate: `deno_core` type-checks.

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
- **No facade may swallow failures.** `crates/ffi/src/guard.rs:4-19` turns a
  panic into `R::default()`, which makes an engine bug look like a plausible
  `0`/`false`. The host-facing boundary uses `Maybe`/`MaybeLocal` with a context
  parameter instead, and reserves aborts for invariant violations.
- **Shapes are declared, not implied.** Every shape facade states its tier in its
  header, so nobody mistakes "changes at the class-name level" for "links
  unchanged".

## 10. Build order

Engine side: (1) L1 roots — done; (2) platform + task runner; (3) snapshot +
external references + per-isolate/context data slots; (4) module resolver as a
host trait, unbound scripts, code cache, script origins. Then, in the order the
shim histogram implies: structured frames and termination, host memory, inspector
and source maps, traced host objects, structured clone.

Bridge side: (1) signature-compatible Rust face; (2) grow the surface from the
items `deno_core` names, in call order; (3) point the local `deno/` checkout at
the crate and run a script; (4) migrate, then delete.

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
3. **Sealing `slag::api`** — `Local::value`, `Isolate::agent`, `Local::as_object`
   name `crux`/`runtime` types that are reachable but not yet promised.
4. **`jsc`'s fate** — kept for now (§2); whether it grows the missing typed-array
   predicates or is left alone is undecided.
