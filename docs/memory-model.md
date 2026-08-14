# slag memory model (ECMAScript ch. 28) and the thread-per-agent design

Phase 17's scope decision (PLAN): ES2026's memory model only becomes
observable with multi-agent execution — workers sharing a
`SharedArrayBuffer`. The base engine runs one agent, so this document maps
ch. 28 onto the thread-per-agent design that the experimental `workers`
cargo feature implements, and records what the single-agent build guarantees
today.

## 1. Shared Data Blocks (ch. 28.2)

A Shared Data Block is the `[[ArrayBufferData]]` of a `SharedArrayBuffer`:
raw bytes that agents on separate threads read and write. In slag the block
is `crux::typed_array::SharedBuffer`:

- **Single-agent build** (`workers` feature off): the block is an
  `Rc<RefCell<Vec<u8>>>`. Access is borrow-checked (`read`/`write` copy
  through the `RefCell`), there is no contention, and the `atomic_*`
  operations are plain read-modify-writes. This is the "single-agent
  behavior unchanged" exit criterion: nothing in this mode is observable
  across threads because there is only one thread.
- **Workers build** (`workers` feature on): the block is an
  `Arc<[AtomicU64]>` plus an `Arc<AtomicUsize>` length — 8-byte-aligned,
  `Send + Sync`. Byte access goes through the data pointer; the Atomics
  operations go through real `AtomicU8/16/32/64` accesses at naturally
  aligned offsets (Int32Array elements are 4-aligned, BigInt64Array 8-aligned
  because the block base is 8-aligned). The same `SharedBuffer` value is
  cloned into every agent that views the buffer, so all agents observe the
  same bytes.

Byte blocks are allocated zero-filled (`SharedBuffer::new`). Growth
(`SharedArrayBuffer.prototype.grow`) replaces the block and length in the
buffer's `BufferState`; views created before the grow keep the old block and
their captured lengths, which matches the spec's requirement that a grow
never races with live views.

## 2. Agent Signifiers (ch. 28.3)

Each `Agent` has a globally unique `[[Signifier]]` (`agent.signifier`, a
counter), plus the host-dependent `[[LittleEndian]]`, `[[CanBlock]]`
(`false` on the main agent, `true` on workers), and `[[IsLockFree1/2/8]]`.
`Atomics.isLockFree` answers from those fields. Signifiers only become
observable through the memory model's `ChronologicalOrder` (not directly
exposed); they exist so the model can reason about which agent wrote a byte.

## 3. Executions: Read, Write, Read-Modify-Write (ch. 28.4)

An execution is a set of events — Read/Write/ReadModifyWrite — plus the
partial orders `SynchronizesWith`, `HappensBefore` and `ChronologicalOrder`.
slag maps the shared-memory operations onto events as follows:

| Language operation | Event(s) |
| --- | --- |
| TypedArray element read on a shared buffer | Read (non-atomic) |
| TypedArray element write on a shared buffer | Write (non-atomic) |
| `Atomics.load` / `Atomics.store` | Read / Write with SeqCst order |
| `Atomics.add/sub/and/or/xor` | ReadModifyWrite (SeqCst) |
| `Atomics.exchange` / `Atomics.compareExchange` | ReadModifyWrite (SeqCst) |
| `Atomics.wait` / `Atomics.notify` | the wait/notify synchronization edge |

Non-atomic typed-array accesses on a shared buffer are data races unless
synchronized (section 5); the Atomics operations are always atomic events.

## 4. Synchronizes-with (ch. 28.5)

A synchronization edge (`sw`) is created by:

1. **`Atomics.notify` → `Atomics.wait`**: a `notify` that wakes a suspended
   waiter synchronizes with that waiter's subsequent reads. In slag the edge
   is the OS-level `Condvar` wake-up: the notifier (after releasing the
   wait-registry lock) `notify_one`s, and the woken waiter observes the
   notifier's prior SeqCst writes because the mutex/condvar pair provides the
   required release/acquire ordering. This is the mechanism the stress tests
   rely on for message passing: store a value with `Atomics.store` (SeqCst),
   then `notify`; the waiter's `wait` returns and the subsequent (plain) read
   sees the value.
2. **SeqCst ordering**: all Atomics operations use `Ordering::SeqCst`, which
   in Rust orders every SeqCst access on the same and different locations
   (`sequenced-before` edges between SeqCst events form a single total
   order). Per ch. 28.5, if an event `B` is `sequenced-before` `A` on the
   same thread and `A` synchronizes with `C` on another thread, then
   `B` happens-before `C`. The SeqCst fences the model mentions are
   subsumed: every Atomics operation is itself SeqCst.

The wait/notify registry (`builtins::atomics::WAIT_REGISTRY`) is keyed by
`(block_id, byte_offset)` — the byte block address, not the JS object — so
two agents holding different `SharedArrayBuffer` objects over the same block
wake each other correctly.

## 5. Happens-before and data races (ch. 28.6, 28.7)

`HappensBefore` is the transitive closure of `SynchronizesWith` and
`sequenced-before` (program order within an agent). A data race is two
non-atomic accesses to the same memory location that are not ordered by
`HappensBefore`, where at least one is a Write. slag's guarantees:

- **No tear**: Atomics accesses of 1/2/4/8 bytes are single hardware atomic
  operations on the aligned word; a torn read (half an old value, half a
  new) is impossible.
- **No invented reads / no invented writes**: the engine only performs the
  accesses the program expresses; the compiler does not speculative-load
  shared bytes, and the Rust `unsafe` byte copies are single
  `copy_nonoverlapping` calls, not loop-unrolled read-ahead.
- **Data races are bugs**: under `workers`, a racing plain read/write is
  `unsafe`-adjacent and its result is undefined by the model (in practice
  hardware-ordered). The tests never rely on racy access; correct programs
  synchronize via the section-4 edges.

## 6. Write buffers (ch. 28.9) and flushing

The spec lets an agent hold a write buffer of its own writes and flush it
under the memory-model rules. slag's Rust implementation delegates to the
hardware/compiler memory model: SeqCst accesses on the `Atomic*` types
compile to locked/`mfence`-style instructions (x86) or the equivalent, and
the condvar wake-up publishes prior writes. There is no slag-level write
buffer: every Atomics store is immediately visible to the memory system, and
the `SeqCst` ordering in Rust provides the total order ch. 28.9's flushed
execution requires. A future port to a weaker compiler target must audit the
`atomic_*` implementations (they are all in `crux/src/typed_array.rs`) for
the ordering arguments used.

## 7. `Atomics.wait`, `HostResolveJobQueue`, and suspension (ch. 28.11, 9.4.4)

`Atomics.wait` suspends the calling agent. `[[CanBlock]]` is false on the
main agent, so `wait` throws `TypeError` there (matching the spec:
"AgentCanSuspend() is false" → TypeError) and `waitAsync` is the only
non-blocking path — it returns `{ async: true, value: promise }` and the
runtime resolves the promise immediately (the main agent never actually
suspends, so there is no later notify to await).

On a worker agent (`can_block = true`), `wait` runs the ch. 28.11 loop:

1. Read the location (SeqCst). If it differs from the expected value, return
   `"not-equal"` (this also closes the race where a `notify` fired before the
   waiter registered — the value check catches it).
2. If the timeout expired, return `"timed-out"`.
3. Register a `WaiterEvent` under `(block_id, byte_offset)` in the global
   registry, then block on its `Condvar` (with the remaining timeout).
4. On wake (notify or timeout), remove the event and loop.

The notifier pops up to `count` events under the registry lock, sets each
event's `notified` flag, and `notify_one`s each — the count returned by
`Atomics.notify` is the number of events popped. Registration happens before
each suspension (not once), so a wake-up that raced with registration is
harmless: the flag forces a value re-check, and a later notify reaches the
freshly re-registered event.

`HostResolveJobQueue`-level interaction: `Atomics.wait` blocks the OS thread
that runs the worker's job queue. That is the intent — a worker executing
`wait` has no runnable jobs until it is notified. The main agent's `wait`
throws rather than blocking, so the host's event loop (which drives the main
agent) never stalls.

## 8. What the tests verify

- Single-agent build: the `builtins::atomics` tests check RMW semantics
  (store/load/add/sub/and/or/xor/exchange/compareExchange on Int32 and
  BigInt64, two's-complement wrapping), receiver validation, `wait` throwing
  on the main thread, `waitAsync` resolving `"ok"`/`"not-equal"`, and
  `notify` returning 0.
- Workers build (`cargo test -p runtime --features workers`): a message-pass
  test (worker `Atomics.wait`s on a slot, the main agent stores a value and
  `notify`s, the worker observes the payload) and a SeqCst counter stress
  test (four workers `Atomics.add` 5000 times each; the final count is
  exactly 20000). Both are thread-local to the runtime test process, so they
  are safe under any `--test-threads` setting.
