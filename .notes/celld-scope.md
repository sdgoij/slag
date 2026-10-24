# Celld on Slag: scope for the shared-isolate model

Celld is the second Rust host on the bridge (`v8 = "152.1"`), and the first that
is not `deno_core`. Since the bridge now stands in for 152.1, its compile probe
reduces to one architectural gap: celld's isolates belong to no thread and are
entered by whichever worker takes a lock. This note records what that costs.

## What celld needs, measured

The probe (celld's own workspace, `v8` patched to `crates/v8`, Linux/WSL — its
native dependencies do not build on Windows) went from 564 errors to 560 after
the 152.1 API additions. What remains, by kind:

| Kind | Count | Meaning |
|---|---|---|
| `E0277` `Send`/`Sync` | 357 | `Rc`, `NonNull`, `*mut` in the isolate and handle types |
| `E0425` | 3 | `v8::SharedIsolate`, `v8::Locker` ×2 |
| `E0599` | 1 | `OwnedIsolate::try_into_shared` |

Its architecture (`crates/celld/pool.rs`, `js.rs:2194-2328`) is: one
`v8::SharedIsolate` per isolate, a tokio permit taken before locking, and every
turn runs as `let (locker, cells) = isolate.lock();` — `Locker` derefs to the
isolate (the heap-limit callbacks and `get_slot` are called on it), and
`Global<Context>`/`Global<Function>` live in a struct shared across workers.

## What already fits

- `IsolateHandle` is already `Send + Sync` (`Arc<AtomicBool>`, `isolate.rs:696`);
  the watchdog celld runs on another thread is exactly that.
- `crux::function::with_agent` fires an enter hook on a genuine window
  transition, and `live_agent_entered` **re-registers an agent that moved**,
  replacing the stale address by signifier (`agent.rs:305-317`). The per-agent
  precise roots (M-1) are in `AgentRoots`, not on the stack.
- The conservative stack scan binds to the thread that is running — which is the
  correct thread after a locked entry.

## The four blockers

**1. The handle arena is per-thread.** Every scoped handle is an index into
`store::TABLE`, a `thread_local!`, and its collector registration is a
`thread_local!` flag (`store.rs:176-191`). A `Global`'s payload may name that
table (`Payload::Script { slot, generation }`), so a handle minted on thread A
means nothing on thread B. This is the reason `Global` is `!Send`, and it is
invisible from the error list: it is a correctness issue, not a type one.

**2. Root discovery is per-thread, and a moved isolate leaves a stale entry.**
`ROOT_SOURCES` (`heap.rs:1498`) and `LIVE_AGENTS` (`agent.rs:298`) are
thread-local. `live_agent_entered` adds the agent to the *new* thread's list but
cannot remove it from the old one. A collection on the old thread would then
trace an agent that is running on another thread — a cross-thread read of
`AgentRoots`, which is UB before any API work is done. Fixing this is not
optional cleanup; it is a precondition.

**3. `Agent.roots` is an `Rc`, and a `Global` holds a `crux::heap::Pin`.**
`Rc<AgentRoots>` (`agent.rs:1721`) has a non-atomic refcount; `Pin { any:
Option<GcAny> }` (`heap.rs:1435`) is a raw pointer. Either they become `Arc` /
stay raw under a documented lock discipline, or a handle dropped on A while B
runs races. The store entry `Option<Rc<store::Entry>>` in `Global`
(`handle.rs:758`) is the same question in miniature.

**4. `try_into_shared` is an ownership move with a pointer invariant.** `Isolate`
is `NonNull<IsolateInner>` and every `PinScope` carries a copy; the state must
move into the shared wrapper *without moving the allocation* (a `Box` inside the
`Mutex` does that), and every entry must install the TLS the engine reads —
current agent, current realm, and (after blocker 1) the current store.

## Proposed design

- `pub struct SharedIsolate { inner: Mutex<Shared> }` where `Shared` owns the
  `Box<IsolateInner>` and records the resident `ThreadId`. `Send + Sync` by the
  lock, documented like `SharedRef` and `BorrowedBlock` already are.
- `lock() -> Locker<'_>`: `MutexGuard`, installs the thread's TLS for this
  isolate and restores it on drop; `Deref`/`DerefMut` to `Isolate` so celld's
  `locker.get_slot(..)` and `&mut Locker` parameter keep their shape.
- `OwnedIsolate::try_into_shared(self) -> Result<SharedIsolate, Self>`; the
  refusal cases celld asserts cannot fire there (entered, no C++ heap, no weak
  handles) are exactly the ones this bridge can answer honestly.
- Handle arena: move `store::TABLE` behind a `CURRENT_STORE` TLS pointing at
  storage owned by the isolate, so the arena travels with it. This is the
  largest mechanical change and it is what makes `Global: Send` true rather than
  asserted.
- Registries: entries carry the resident thread; `live_agent_roots()` and root
  sources answer only for the current thread; the entry installs and the old
  thread's entry is dropped on move.
- `unsafe impl Send for Global<T>` (and `Weak`/`TracedReference` if the probe
  asks) with the contract rusty_v8 #2045 states: a persistent handle may travel,
  but touching it requires the isolate's locker. `Rc<Entry>` → `Arc<Entry>`.

## Staged plan

1. **Handle-arena ownership** — per-isolate store behind `CURRENT_STORE`,
   installed by `PinScope`/context entry; no API change. Gate: the full
   workspace suite plus the Deno harnesses (Deno is one isolate per thread and
   must be unaffected).
2. **Residency-aware roots** — thread-tagged registries, stale-entry removal on
   move, a test that moves an isolate and collects on both threads with the
   verifier on.
3. **`SharedIsolate` / `Locker` / `try_into_shared` / `thread_safe_handle`** —
   plus `Send` impls. Tests: create on A, lock on B, evaluate a script, drop on
   B; two threads taking turns on one isolate under the lock.
4. **Celld** — compile, then run its own gates (`cargo test`, `clippy -- -D
   warnings`, as its Dockerfile does), then a build script mirroring
   `tools/build-deno.py`.

Steps 1-2 are engine-internal and testable without celld; step 3 is the API
surface; step 4 is the payoff and the least uncertain.

## Risks and open questions

- **Step 2 is the one that can go wrong quietly.** The failure mode is a stale
  root traced by the wrong thread, which the current verifier would not
  necessarily catch; the test in step 2 needs the A2 barrier verifier and a
  two-thread schedule, not just a green suite.
- `Pin`'s drop path: whether it touches TLS decides if it needs `Arc` or a
  lock-scoped rule.
- `Payload::Script`'s slot/generation pair is per-thread today; a persistent
  script handle that crosses threads needs the store it names.
- Microtask queues and the inspector session are per-isolate state celld drives
  from several threads; they follow the store's rule, not a separate one.
- Deno must keep working unchanged — it never shares an isolate, so steps 1-3
  should be invisible to it; the harnesses are the gate.

Rough size: steps 1-2 are the bulk (roughly a week, with step 2 the risky half);
step 3 is a few days; step 4 a day or two. Nothing here changes what the engine
*computes* — it changes where its state lives and who may touch it.
