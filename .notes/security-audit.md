# Security Audit — Slag JavaScript Engine

**Audit date:** 2026-10-23
**Commit:** `883af3dd` (main), clean working tree at audit start
**Toolchain:** Rust 1.96.0, release build via `tools/build.sh -p cli`
**Scope:** the 17 workspace crates (~400k LOC, 1,638 `unsafe` occurrences). Vendored
trees (`v8/`, `deno/`, `test262/`, `waspec/`, `scratch/`) were treated as out of
scope except where they feed the build.

**Method:** static review of each attack surface, targeted PoCs against the
release binary, and `cargo audit` against the 2026-10-23 RustSec database.

**Threat model:** an embedder (Deno, the `slag` CLI, or a JSC C host) executes
attacker-supplied JavaScript, WebAssembly, and regular expressions. Rust panics,
native stack overflow, allocator abort, use-after-free, and out-of-bounds access
are security issues.

**Result:** 0 known vulnerable dependencies, 6 critical memory-safety defects
(1 JIT/GC, 5 in the JSC C API), and 12 high-severity crash/DoS paths. The engine
has unusually strong invariants and tooling; `--gc-verify` found the JIT bug, but
untrusted-input hardening is uneven.

Severity scale used below:

- **Critical** — memory unsafety (UAF, OOB, type confusion, arbitrary write), RCE,
  or sandbox escape.
- **High** — reliable process abort/hang from small untrusted input in the
  default build, or memory safety reachable under a supported configuration.
- **Medium** — resource amplification, latent UB, capability leakage, supply-chain
  weakness.
- **Low** — defense-in-depth and hardening.

---

## Executive summary

| Severity | Count | Character |
|---|---|---|
| Critical | 6 | JIT/GC untraced slots; C-API lifetime/ownership |
| High | 12 | One-line crashes/aborts/hangs in the default build |
| Medium | 12 | Resource amplification, latent UB, capability leakage, supply chain |
| Low | 9 | Hardening and defense-in-depth |

The most important result is **C-1**: the JIT sizes its working stack from a step
model that omits `LoadLocal`, `CreateArrow`, and other pushing steps, so live
`Value`s can sit above `vm.stack.len()` and evade tracing. The resulting missed
write barrier was reproduced with `--gc-stress`; in production the same schedule
is a use-after-free of young closures.

## Remediation status (2026-10-23)

Fixed and validated in the first remediation pass:

- **C-1** — added the missing `max_stack_usage` arms (`LoadLocal`,
  `CreateFunction`, `CreateArrow`, `NewTarget`, `RegExpLiteral`), sampled the
  transient `CallFastSlotStore`/`CallFastGlobalStore` peak, and added the
  write barrier to `array_extend_dense`. The `--gc-stress` PoC no longer trips
  the barrier verifier.
- **H-1** — parser, `JSON.parse`, `JSON.stringify`, proxy forwarding,
  `Array.prototype.flat`, and `/v` nested classes now have depth budgets. The
  measured safe parser bound on the 1 MB Windows main-thread stack is **96**;
  deeper legal code is rejected with a `SyntaxError` instead of aborting.
- **H-2** — the matcher has an adaptive step budget and a choice cap; the
  empty-body repeat clamp removes the `{4294967295}` loop. Budget exhaustion
  now propagates through the checked exec/search API and reaches JS as
  `RangeError: Regular expression is too complex`.
- **H-3** — month 13 in Chinese/Dangi formats numerically instead of indexing
  past the 12-entry name table.
- **H-4** — `CreateListFromArrayLike` (Reflect, proxy `ownKeys`, `apply`) caps
  its length at 2^20 and no longer preallocates from `ToLength`.
- **H-5** — BigInt exponentiation and `asUintN`/`asIntN` are capped at 2^30
  bits and throw `RangeError`.
- **H-7** — `Array.prototype.sort` uses a stable merge sort driven by a
  fallible comparator; an inconsistent JS comparator no longer panics.
- **H-8** — the GC drop condition is `||`, so spilled `JsObject` storage and the
  wrapper-table entries are released.
- **H-9** — `ACTIVE_RUNS`, `with_agent`, and the JSC current-context slot are
  restored by `Drop` guards.
- **H-10** — `SharedBuffer` bounds math uses `checked_add`; the typed-array
  same-type copy uses saturating arithmetic.
- **H-6** — `wasm::Memory::new`/`TableInst::new` reject declarations above the
  host caps (1 GiB of memory, 4 Mi elements) with a link error, `grow` caps at
  the same ceiling, and the JS-API `memory_limits` validates `maximum` as well
  as `initial`.
- **H-12** — validation precomputes rec-group ends and the function type index,
  uses a `HashSet` for duplicate exports, caps types/exports/`array.new_fixed`,
  and bounds per-body and module-wide local expansion.
- **C-2** — context teardown is deferred to the outermost API boundary and
  reference-counted, `JSStringGetCharactersPtr` points into a stable boxed
  string, the property-name accumulator validates its pointer, string/value
  retains and `JSValueUnprotect` release their entries, the `finalize` callback
  is wired through `HostOps::finalize`, `JSClassRelease` clears only its own
  class's callbacks, and a finalize ref resolves the dead object's private-data
  id.
- **H-11** — the CLI no longer installs `fs` by default; `--allow-fs` is the
  explicit unrestricted opt-in and `--allow-fs=ROOT` canonicalizes every path
  and rejects anything outside the root. Reads are capped at 256 MiB and a
  directory listing at 2^20 entries.
- **M-5** — externref conversions are capped at 2^20 live payloads
  (`RangeError`), so the per-agent map cannot grow without bound.
- **M-8 (partial)** — `crates/v8` is `publish = false`, `tools/build-deno.py`
  requires a full 40-hex pin, and the Unicode build validates fixture stems
  before splicing them into generated Rust.
- **M-2** — `ArraySlots::trace`/`trace_dirty` read the element buffer through
  the `RefCell` (`try_borrow`) and clamp the length to the buffer's capacity; a
  live mutable guard aborts the sweep instead of dereferencing a cursor that an
  intervening reallocation may have invalidated. A regression test holds the
  guard across both trace paths.
- **M-10** — `collect_workloads` uses `symlink_metadata` and skips symlinks (a
  symlinked directory can point at an ancestor) and bounds the recursion at 32
  levels.
- **M-11** — embedded raylib assets are materialised with `create_new` under a
  per-process nonce name; an existing path (a pre-planted symlink included) is
  never truncated, and a failed write removes the partial file.
- **Low** — `Heap::alloc` sizes an oversized payload's chunk to fit it (a >1 MiB
  box no longer bumps past `chunk.end`); `Gc::new_in_place` writes the header
  through `addr_of_mut!` instead of forming a reference over uninitialized
  memory; `trace_roots` uses `try_borrow` for the generator tables; typed-array
  index checks use `usize::try_from`; `Store::start_owned` rejects a call whose
  argument count differs from the declared parameters, and the JS-API export
  path converts exactly the declared parameters (missing → `undefined`, extra
  ignored) instead of copying the JS argument vector, which shifted declared
  locals.
- **Low (deferred)** — import matching still ignores the `shared` flag. The
  binary decoder rejects a shared module memory type
  (`decode_memory_limits`), so the only shared type the store can see is a
  host-created `WebAssembly.Memory`; enforcing equality would reject the
  intentional shared bridge without enabling a conformant shared module.
  Revisit when the decoder accepts shared limits.
- **M-4** — `Symbol.replace` now runs the single spec loop (22.2.7.3 steps
  13a-13l): each match is replaced before the next exec instead of collecting
  every exec result first. Memory is bounded by the output string rather than
  by the match count, and a functional replacer's `lastIndex` write now affects
  the next match. A regression test pins the interleaving.
- **M-3 (deferred)** — the eager `maxByteLength` pre-allocation exists only in
  the experimental `workers` feature: `byteblock`'s `new_with_capacity` ignores
  the capacity in the default build. Under `workers` the block is one immutable
  `Arc<[AtomicU64]>` shared by every view, so lazy growth needs a shared
  indirection that the worker memory model (M-12) has to settle first.
- **M-8 (remainder)** — the Deno reconcile now logs every lock entry it moved,
  the build runs `cargo build --locked` so nothing resolves beyond the
  reconciled lock, and an overridden `DENO_REPO` is announced instead of being
  silently trusted.
- **Low** — `padStart`/`padEnd` fill the gap directly into the result buffer
  instead of materializing a second filler copy.
- **M-8 (actions)** — every workflow `uses:` is pinned to a 40-hex commit with
  the moving tag kept as a trailing comment, so a retagged action cannot change
  what CI runs.
- **M-9** — CI gained a `gc-stress` job (Array built-ins and array expressions
  under `--gc-stress --gc-verify`, the collection class that exposed C-1), a
  `cargo audit` job pinned to `cargo-audit 0.22.2`, and a Miri job over the
  lexer and syntax crates that runs only on manual dispatch (a
  `workflow_dispatch` input), so push/PR runs never wait on it. Miri cannot run
  `crux` (the collector reads the native stack), and the regexp/parser suites
  need minutes of interpreted matching per test. Fuzzing remains absent.
- **Miri investigation (fixed)** — `regexp::fold_class` no longer scans all
  1,114,112 code points on first use; `crates/regexp/build.rs` emits the reverse
  fold index at compile time, so the lookup is a binary search over static data
  with no lazy initialization or global lock, and a test recomputes the scan
  and compares every entry.
- **Low (JIT i-cache)** — `ExecutableCode::new` now flushes the instruction
  cache after copying the code and raising the protection. x86-64 is coherent
  and compiles the call away; linux-aarch64 runs the `dc cvau` / `ic ivau`
  sequence (compiled and linted for that target).
- **Low (snapshot decode)** — reviewed and covered: `header` validates the body
  length and the trailing magic, every body count is checked against the bytes
  remaining, and `Reader` is bounds-checked. A deterministic bit-flip sweep
  over the header, context table and first record pins that a corrupted blob
  returns an error and never panics.
- **Raylib asset root** — `rl.load*` disk reads are capability-gated: embedded
  assets always load, and `AssetAccess::{EmbeddedOnly, Unrestricted, Root}`
  governs a path. The CLI defaults to `EmbeddedOnly` and takes
  `--allow-rl-assets[=ROOT]`; a root canonicalizes the path and rejects escapes.
- **`ffi::guard` poisoning** — a panic at the C boundary now sets a per-thread
  poison, and later guarded calls return `R::default()` without running (the
  isolate's state is unknown). `is_poisoned`/`unpoison` are exported and
  re-exported by `crates/jsc` for a host that rebuilds its state.

Outstanding from this report: the JSC string/value/object ref namespace, the
`crates/v8` per-thread realm slot, the Deno build's one-time unlocked reconcile
(now logged and followed by a locked build), the remaining Medium items
(`current_agent_mut` aliasing, eager resizable-buffer allocation under
`workers`, worker memory atomics), and the fuzzing gap.

Validation for the first pass: `cargo clippy --workspace --all-targets -- -D
warnings` is clean; `cargo fmt --all -- --check` is clean; 1,062 runtime unit
tests and the parser/crux/byteblock/regexp suites pass; the full test262
`language` area passes 23,726/23,726, `built-ins` passes 23,821/23,821, and a
`--gc-stress` sample of 159 Array fixtures passes with the verifier on. The
original PoCs for C-1, H-1, H-3, H-4, H-5, H-7, H-8, H-9, and H-10 no longer
crash, hang, or trip the GC verifier.

Validation for the second pass: clippy and rustfmt stay clean; the wasm core
suites pass 64,594 assertions (top-level 20,662; gc 654; multi-memory 912; simd
25,990; memory64 8,709; bulk-memory 7,485; exceptions 105; relaxed-simd 77),
the JS-API suite passes 1,001 tests, and the runtime/jsc/ffi tests pass. The
oversized-memory module now fails instantiation with a `LinkError` naming the
host limit, `Table.grow(2**32-1)` raises `RangeError`, and growing the string
table by 2,000 entries leaves `JSStringGetCharactersPtr` stable.

Validation for the third pass: clippy and rustfmt stay clean; 1,064 runtime
tests (with `fs`) and 18 CLI tests pass; the full RegExp suite passes
1,899/1,899; the WASM JS-API suite still passes 1,001 tests. `fs` is absent
without `--allow-fs`, a rooted read inside the root succeeds, `../` and an
absolute outside path are rejected, and the ReDoS probe now returns a
catchable `RangeError` while a normal match still succeeds.

Validation for the fourth pass: clippy and rustfmt stay clean; 1,063 default-
feature runtime tests and 264 crux tests pass in release; the wasm crate's 41
tests and the 28 WASM JS-API runtime tests pass; `cargo check`/`clippy -p
runtime --features raylib` is clean; a dense-array `--gc-stress` run (200 rounds
of append, replace, and truncate) completes with the verifier on; and the new
arity test proves `first(7) → 7` and `local(5, 99) → 0` (the extra argument no
longer lands in the declared local slot).

Validation for the fifth pass: clippy and rustfmt stay clean; 1,064 runtime
lib tests pass in release; the full RegExp area passes 1,899/1,899 and every
`*replace*` fixture passes 212/212; `"a".repeat(10_000_000).replace(/(?:)/g,
"")` completes and prints its 10,000,000-unit result instead of materializing
10M match arrays.

Validation for the sixth pass: `python -m py_compile tools/build-deno.py`
succeeds and `--dry-run` emits `cargo ... build -p deno --locked --release`;
clippy and rustfmt stay clean; the 24 string unit tests and all 35 `*pad*`
fixtures pass.

Validation for the seventh pass: the new `gc-stress` commands were run locally
first — 33 Array built-in fixtures and 25 array-expression fixtures pass under
`--gc-stress --gc-verify` inside the 15s deadline — and every workflow `uses:`
is pinned to a 40-hex commit.

Validation for the eighth pass: `cargo audit` reports no advisories over 134
crate dependencies (exit 0); `cargo +nightly miri test -p lexer --lib` passes 59
tests and `-p syntax --lib` passes 11. The `crux`, `parser`, and `regexp`
suites were excluded from the Miri gate: `crux` needs the native stack, and a
bounded `parser` run showed `regexp::engine::fold_class` still advancing after
millions of basic blocks — it enumerates the whole code point space once to
build the case-folding table, and `parses_literals_and_identifiers` reaches it
through a regexp literal. The `regexp` suite stalled on the same table (since
removed; see the fold-table entry above).

Validation for the ninth pass: 41 regexp tests (including
`generated_fold_tables_match_a_runtime_scan`) and 88 parser tests pass in
release; the RegExp area passes 1,899/1,899 and `*replace*` 212/212; clippy
(`-D warnings`) and rustfmt are clean. The Miri job now covers the lexer and
syntax crates and only runs from a manual `workflow_dispatch` with its input
set, so push/PR runs do not wait on it.

Validation for the tenth pass: clippy is clean for the default workspace and
for `-p runtime -p cli --features raylib`, and rustfmt is clean; `cargo check`
and `clippy -p jit --target aarch64-unknown-linux-gnu` compile the flush path;
the raylib surface test (with the new policy assertions) passes, as do the 13
`ffi` tests (including the poison test) and 1,068 runtime lib tests.

---

## Critical findings

### C-1 — JIT working-region undercount leaves live GC values untraced (reproduced)

**Locations:** `crates/runtime/src/ir.rs:24136` (`max_stack_usage`),
`crates/runtime/src/jit.rs:7058` (`run_jit_body`),
`crates/runtime/src/ir.rs:4011` (`Vm::trace`),
`crates/crux/src/object.rs:1680` (`array_extend_dense`)

`max_stack_usage` has no arms for `Step::LoadLocal`, `CreateArrow`,
`CreateFunction`, `CreateArguments`, `NewTarget`, or `RegExpLiteral`, even though
`emit_step` pushes a slot for each (`crates/jit/src/compiler.rs:5933`).
`RunRegBody` also resets `depth` before sampling its transient use.
`run_jit_body` sizes the working region as
`stack_usage + JIT_STACK_SLACK(16) + JIT_INLINE_HEADROOM(4)` and resizes
`vm.stack` to exactly that length; `Vm::trace` walks the `Vec` by length, so every
slot above `work_len` is invisible to the collector.

**Reproducer** (JIT is on by default):

```js
function f(){ let s=0; for(let i=0;i<50000;i++){ let a=[()=>1,/* 40 closures */]; s+=a.length; } return s; }
f();
```

```text
$ target/release/slag.exe --gc-stress jit_arr.js
thread 'main' panicked at crates/crux/src/heap.rs:2897:
write-barrier miss: 1 old box(es) hold a young reference the barrier never recorded
offenders: [("crux::object::ArraySlots", "crux::function::Function")]
```

The array/slots box is promoted while the closure values remain young because
they were above the traced stack length; `array_extend_dense` bulk-extends
without a write barrier, so the young closure is never recorded. In a
non-`--gc-stress` run the verifier is off and the young function can be swept
while the array still references it — a use-after-free/type-confusion primitive,
not merely a verifier complaint.

**Fix:** add every pushing step to `max_stack_usage`, sample `max` inside
transient-raising arms, add a `debug_assert` in `emit_step`/`opt_lower` that
`sp − base ≤ body.max_stack`, clamp at body entry against `buf_end`, and add a
per-element barrier (or a proven young-array fast path) in
`array_extend_dense`.

### C-2 — JSC C API lifetime cluster

`crates/jsc` is not in the default workspace build, but
`cargo build -p cli --features jsc` ships it as a drop-in JavaScriptCore C API.

| ID | Location | Defect |
|---|---|---|
| C-2.1 | `crates/jsc/src/context.rs:74-84`, `:261-263` | `JSGlobalContextRelease` drops the boxed isolate unconditionally; a callback can release the context it is executing on, and `try_call` continues on freed state. UAF. |
| C-2.2 | `crates/ffi/src/tables.rs:66-89`, `:139-146`; `crates/crux/src/heap.rs:1497-1521` | **Withdrawn during remediation.** The crux heap is thread-local and shared by every agent on the thread, so the thread-local root tables do not outlive the heap; a released context's boxes remain live heap objects. Cross-context handle confusion is still an API-level Medium issue, not a UAF. |
| C-2.3 | `crates/jsc/src/string.rs:69-77`; `crates/ffi/src/tables.rs:121-126` | `JSStringGetCharactersPtr` returns a pointer into a `HashMap` node. Small strings are stored inline in the value; any later insertion that rehashes/grows the map moves the node and dangles the pointer. |
| C-2.4 | `crates/jsc/src/object.rs:669-679` | `JSPropertyNameAccumulatorAddName` reconstitutes `&'static mut Vec<JsString>` from any pointer with no registry or liveness check. A forged or stale accumulator is an arbitrary write. |
| C-2.5 | `crates/jsc/src/context.rs:250-258`; `crates/jsc/src/refs.rs:57-73` | `Retain` is a liveness check, not a refcount; `JSValueUnprotect` never unpins. Two-owner JSC usage frees objects still referenced by the second owner, while `OBJECTS` grows forever. |

**Fix:** refcount/poison isolates and defer teardown to the outermost call, key
pins by isolate (and reject addresses outside the collecting arena), store strings
in stable per-ref allocations, register accumulators for the callback duration,
and implement real retain/release counts.

---

## High-severity findings

### H-1 — Unbounded native recursion from untrusted input (reproduced)

Every one of these aborts the process with
`thread 'main' has overflowed its stack` (uncatchable) from a tiny input:

| Site | Input |
|---|---|
| `crates/parser/src/expr.rs:49` and the `parse_primary` → `parse_paren_contents` cycle | `'(' * 200_000` |
| `crates/runtime/src/builtins/json.rs:151,209,250` | `JSON.parse('['.repeat(200000)+']'.repeat(200000))` |
| `crates/runtime/src/builtins/json.rs:901,981` | 300k-deep object chain passed to `JSON.stringify` |
| `crates/crux/src/proxy.rs:389,421,487` | 200k chained trap-less proxies, then `p.x` |
| `crates/runtime/src/builtins/array.rs:1377` | `let a=[]; a[0]=a; a.flat(Infinity)` |
| `crates/regexp/src/parse.rs:1325,1388` | 200k nested `/v` classes |

A parser depth counter exists nowhere; a VM stack guard (`stack::enter_js`) only
runs on JS activation, so host-side recursion is unguarded.

**Fix:** depth counters returning `SyntaxError`/`RangeError` (the measured safe
bound on a 1 MB main-thread stack is 96 for the paren/array path), an
explicit-stack or depth-bounded `Drop` for the AST, and a visited/depth budget
for proxy forwarding and `flat`.

### H-2 — RegExp has no step budget; quantifier bounds are unbounded (reproduced)

`crates/regexp/src/engine.rs:460-550,588-629`;
`crates/regexp/src/parse.rs:354-371`.

- `/(a|aa)+$/.test("a".repeat(45)+"b")` exceeded 20 seconds (exponential
  backtracking; memoization only covers simple atoms).
- `/(?:a?){4294967295}/.test("")` exceeded 15 seconds; each empty iteration pushes
  an `Rc` choice snapshot, so the bound can also exhaust memory.

**Fix:** add a step/deadline budget to `Matcher` (throw `RangeError`), memoize
`(node,pos)` for deterministic sub-NFAs, and clamp a quantifier over an
empty-matching body to 1. **Status:** the budget and the empty-body clamp are
implemented; the budget currently returns a non-match, so threading a
`RangeError` through the exec protocol remains.

### H-3 — `Intl.DateTimeFormat` indexes a 12-entry table with month 13 (reproduced)

`crates/runtime/src/builtins/intl/date_time_format.rs:1973`.

```js
new Intl.DateTimeFormat("en",{calendar:"chinese",month:"long"})
  .format(new Date(Date.UTC(2020,5,1)));   // panic: len is 12 but index is 12
```

Chinese/Dangi leap years have 13 months; the leap-month special case only covers
numeric widths.

**Fix:** select by `calendar_months_in_year` or fall back to the numeric form for
months > 12.

### H-4 — `ToLength`-sized `Vec::with_capacity` aborts the process (reproduced)

`crates/runtime/src/builtins/reflect.rs:151`, `crates/crux/src/proxy.rs:605`,
`crates/runtime/src/builtins/function.rs:776`.

```js
Reflect.apply(function(){}, null, {length: 2**53-1});
// memory allocation of 72057594037927928 bytes failed (abort)
```

`length` is `ToLength` (≤2^53−1) and is passed straight to `with_capacity`. The
proxy `ownKeys` and `Function.prototype.apply` paths share the pattern.

**Fix:** never pre-size from a JS length; use growth or `try_reserve` mapped to a
`RangeError`.

### H-5 — BigInt exponentiation is unbounded (reproduced)

`crates/crux/src/bigint.rs:135-152`.

```js
(1n<<10000000n)**4000000000n;
// memory allocation of 65536000024 bytes failed (abort)
```

`exponentiate` accepts any `u32` exponent on an arbitrarily large base;
`left_shift` accepts an `i64` count.

**Fix:** impose a BigInt bit-length limit (V8 throws `RangeError`) before the
shift/pow.

### H-6 — WASM memory/table sizes are not host-bounded (code-confirmed; table hang reproduced)

`crates/wasm/src/exec.rs:238` (`min_pages * PAGE_SIZE` unchecked), `:303`
(`vec![init; min as usize]`), `:322` (`grow`); `crates/wasm/src/valid.rs:410`
(memory64 min ≤ 2^48 pages).

A valid module with `(memory 65536)` allocates 4 GiB (bypassing the JS
`MAX_BYTE_LENGTH` 1 GiB policy); memory64 allows `min` up to 2^47 pages (8 EiB) or
wraps `u64` at 2^48. Table32 allows 2^32−1 entries (68 GB), and
`Table.prototype.grow` has no host cap:

```js
new WebAssembly.Table({element:"externref", initial:0}).grow(2**32-1); // >20s, 68GB
```

**Fix:** validate page/entry counts against an embedder cap, use
`checked_mul`/`try_reserve`, and surface `LinkError`/`RangeError` instead of
aborting.

### H-7 — A JS comparator violating total order panics the engine (reproduced)

`crates/runtime/src/builtins/array.rs:2234` (and `typed_array.rs:1850`).

```js
let a=[]; for(let i=0;i<2000;i++)a.push(i); a.sort(()=>Math.random()-0.5);
// panicked at core/src/slice/sort/shared/smallsort.rs:860:
// user-provided comparison function does not correctly implement a total order
```

A comparator is allowed by the spec to be inconsistent; Rust's stable sort is not.

**Fix:** implement the merge internally or catch the panic and throw a JS error.

### H-8 — GC drop short-circuit leaks every non-trivial `JsObject`

`crates/crux/src/heap.rs:373` vs the documented contract at `heap.rs:60`:

```rust
if T::DROP_MAY_RELEASE && (*payload).drop_releases_heap() {
```

`JsObject::DROP_MAY_RELEASE = false` (`crates/crux/src/object.rs:1060`), so
`drop_in_place` never runs for any `JsObject`; `drop_releases_heap` is dead code
for its only implementor. Spilled property vectors, `property_index`, and
`private_elements` leak per dead object, and `Drop for JsObject` never queues
`DEAD_WRAPPER_IDS`, so the wrapper tables never evict. The operator should be
`||`. Long-running hosts grow without bound.

### H-9 — Non-RAII TLS state across caught panics

`crates/runtime/src/ir.rs:3582-3611` (`with_jit_run`/`with_leaf_run` push a raw
`*const Vm`/`*const CompiledBody` and pop only on the normal path),
`crates/jsc/src/context.rs:92-98` (`with_current`),
`crates/runtime/src/agent.rs:272-296` (`LIVE_AGENTS` raw pointer),
`crates/crux/src/function.rs:89-94` (`CURRENT_AGENT`).

`ffi::guard` (`crates/ffi/src/guard.rs:7`) and the test262 sweep
(`crates/test262/src/bin/sweep.rs:555`) catch panics and continue. The next
collection then traces a freed body/Vm or attributes callbacks to a stale realm.

**Fix:** make every push a `Drop` guard, pop by identity, and poison the context
after a caught panic.

### H-10 — `SharedBuffer` bounds checks wrap; `TypedArray.set` can underflow

`crates/byteblock/src/lib.rs:636,676,715` use `offset + len > byte_length`;
release builds wrap. `crates/runtime/src/builtins/typed_array.rs:1692` computes
`end - start` after a resizable source shrinks below `byte_offset`, wrapping the
length. A wrapped value can pass the guard and reach `data.add(offset)`. No
current caller was proven to supply the wrapped offset, but this is the crate's
public API and the only guard.

**Fix:** `offset.checked_add(len)`, `saturating_sub`, and a `try_reserve` for the
copy.

### H-11 — CLI grants unrestricted filesystem access by default

`crates/cli/src/main.rs:317` calls `context.install_fs()` for every script;
`crates/runtime/src/embed.rs:978-1101` exposes
`readFileSync`/`writeFileSync`/`readdirSync`/`statSync` over absolute paths with
no root and no size limit. `slag untrusted.js` can read `~/.ssh/id_rsa` or
overwrite `~/.bashrc`. The `raylib` feature additionally passes script strings to
`loadImage`/`loadTextureFromFile`/`loadSoundFromFile`/`loadMusicFromFile`/
`takeScreenshot` without canonicalization
(`crates/runtime/src/raylib.rs:1829,2404,2425,2518`). This is Node-like behavior,
but it is a capability leak for an engine advertised as embeddable.

**Fix:** `--allow-fs[=ROOT]` (deny by default), canonicalize under the root, cap
read sizes, and put raylib asset loads/screenshot/clipboard behind the same grant.

### H-12 — WASM validation/decoding is superlinear

`crates/wasm/src/valid.rs:97` (`rec_group_end` per type with a linear
`rec_group_of` at `module.rs:236`), `:2188` (`func_type_of` uses `.nth`), `:469`
(duplicate export scan), `:1535` (`array.new_fixed` loops over an untrusted `u32`
even in unreachable code), `binary.rs:846` (per-body `MAX_LOCALS` expansion). A
few hundred KB of types/exports/locals can hang `WebAssembly.validate` for hours.

**Fix:** precompute type→group and function→type maps, use sets for exports, bound
`array.new_fixed`, and bound total locals per module.

---

## Medium and low findings

**Medium**

- **M-1** `runtime/src/context.rs:25-36` — `current_agent_mut` fabricates
  `&'static mut Agent` from TLS; nested host closures can alias the engine's own
  `&mut Agent` (UB under `noalias`).
- **M-2** `crux/src/object.rs:710-770` — `ArraySlots::trace` reads
  `elem_ptr`/`elem_len` without borrowing the `RefCell`; a guard held across an
  allocation can leave the cursor on freed memory (suspected, no reachable call
  site found).
- **M-3** `runtime/src/builtins/array_buffer.rs:204,249` — resizable/growable
  buffers eagerly allocate `maxByteLength`; `new ArrayBuffer(0,{maxByteLength:2**30})`
  commits 1 GiB.
- **M-4** `runtime/src/builtins/regexp.rs:1225` — `Symbol.replace` accumulates
  every match array; `"a".repeat(1e8).replace(/(?:)/g,"")` allocates tens of GB.
- **M-5** `runtime/src/builtins/wasm.rs:2483` — externref conversions grow
  `wasm_extern_values` forever; the `u32` token wraps after 2^32 conversions
  (identity confusion).
- **M-6** `crates/jsc` — object refs are pinned forever, `invoke_finalize` has no
  caller, `JSClassRelease` clears all classes' finalizers, and
  string/value/object refs share one untagged numeric namespace.
- **M-7** `crates/v8` — the entered-realm slot is per thread, not per isolate; two
  isolates on one thread can resolve ids in the wrong realm (suspected).
- **M-8** Supply chain — GitHub Actions pinned to mutable tags in the
  release/deploy jobs (`ci.yml:35,42,135,240,242,263,526,540`);
  `tools/build-deno.py:223` runs `cargo update` (no `--locked`) on an env-supplied
  repo/pin; `crates/v8` declares `name="v8"` at `150.4.0` without
  `publish = false`; `crates/unicode/build.rs:52,284-299` splices test262 fixture
  stems unescaped into generated Rust.
- **M-9** Process — CI runs clippy and conformance sweeps but no `cargo audit`,
  Miri, fuzzing, or `--gc-stress` smoke test. The JIT bug in C-1 would have been
  caught by a stress smoke test.
- **M-10** `cli/src/main.rs:856-878` — `--corpus` follows directory symlinks with
  no depth/visited bound.
- **M-11** `runtime/src/raylib.rs:814-825` — embedded assets are written to a
  predictable temp path with truncation, not `create_new` (symlink clobber).
- **M-12** `crates/wasm/src/exec.rs:211-219` + `byteblock` — under `workers`,
  plain `&mut [u8]` views over `Arc<[AtomicU64]>` race with JS `Atomics` (UB in
  the memory model).

**Low**

- `crux/src/heap.rs:1830` — `Heap::alloc` can advance the bump past `chunk.end`
  for a >1 MiB payload (latent).
- `crux/src/heap.rs:498` — `Gc::new_in_place` forms `&mut *raw` over uninitialized
  memory (Miri UB).
- `crux/src/object.rs:5238` — `u64 → usize` truncation in typed-array index checks
  on wasm32.
- `runtime/src/agent.rs:2171` — `trace_roots` uses `borrow()` where neighbours use
  `try_borrow`; a stress collection during a mutable borrow panics.
- `wasm/exec.rs:6692` — import matching ignores the `shared` flag.
- `wasm/exec.rs:3191,3949` — the public `Store` API assumes a validated module and
  exact call arity; violations panic rather than trap.
- `string.rs:660` — `padStart`/`padEnd` transiently allocate ~2× the result.
- `runtime/src/embed.rs:339` — `Context::read_snapshot`'s `is_valid` checks only
  the header; a hostile snapshot cache can still fail deep in decode.
- JIT code pages are W^X-correct, but `crates/jit/src/code_buffer.rs:20-33` never
  flushes the instruction cache; harmless on x86-64, potentially stale on the
  shipped linux-aarch64 target.

---

## M-1 design note: the agent aliasing model (open)

M-1 is not a `current_agent_mut` defect; it is the engine's ownership model.

- `crux::function::with_agent` stores a raw agent pointer in TLS, and skips the
  store when the same *address* is already current. A nested
  `runtime::function::call` therefore leaves the slot holding an ancestor of the
  live `&mut Agent`s (a reborrow has the same address).
- Retagging `&mut Agent` from that ancestor — `current_agent_mut`, or the ECMA
  hook — pops every descendant tag under Stacked Borrows. The frames keep using
  those `&mut`s after the call returns, and LLVM's `noalias` on them was
  violated while the retagged reference was live.
- The collector has the same shape: `live_agent_ptrs()` + `trace_roots` read
  every agent on the thread during *any* allocation. That access runs through
  the registered root pointer, an ancestor, so it invalidates the live `&mut`s
  as well. Passing the agent explicitly to host callbacks, or refreshing TLS at
  each dispatch, is therefore not sufficient on its own: an allocation inside
  the callback still runs a collection.
- The only model-sound fix is for collection-time and host-callback-time access
  to go through interior mutability (`Cell`/`RefCell`/`UnsafeCell`), which a
  `&mut Agent` does not assert uniqueness over. `trace_roots` touches ~100
  fields; some are already interior-mutable (`realms`, `function_realms`, the
  generator tables), but the hottest are plain (`execution_context_stack`,
  `ecma_functions`, `compiled_bodies`, `microtask_queues`).
- Miri cannot validate any candidate: the arena's address-tagged boxes are
  int-to-pointer and fail before an aliasing check runs.

Options:

| Option | Scope | Verdict |
|---|---|---|
| A. Document as debt | none | Zero risk; the aliasing stays on paper |
| B. Explicit agent for host callbacks | ~70 mechanical native signatures + dispatch | Removes the user-controlled re-entry, not the collector |
| C. Interior-mutable traced state, staged | Move the fields `trace_roots` reads behind `UnsafeCell`/`RefCell` | The only complete fix; large and perf-sensitive |
| D. Root-set GC | Roots live outside `Agent`, registered at safe points | Best long term; a GC redesign |

Recommendation: B first (it deletes the `&'static mut` fabrication from the
host surface and makes the remaining paths internal), then C in slices starting
with the coldest containers, with `--gc-stress` and the call benchmarks as the
gate. M-1 should not be marked fixed until C lands.

### Slice 1 landed: `AgentRoots` (the job, promise and generator tables)

Chosen route is D, built in slices: `AgentRoots` is a separately allocated
struct, owned through `Rc`, holding every container the collector traverses.
Because it is a distinct allocation, a live `&mut Agent` says nothing about
its memory, and because every field is `RefCell`, the interpreter and the
collector both reach it through `&AgentRoots`. `collect_with` still receives
`&Agent` from the live-agent registry, so this slice is structural rather than
a complete fix; the registry switch to `*const AgentRoots` is slice 2, and
`trace_roots` keeps its remaining fields until slices 2-4 move them.

Moved in this slice: `promise_jobs`, `generic_jobs`, `timeout_jobs`,
`microtask_queues`, `kept_alive`, `promises`, `promise_resolvers`,
`promise_compound`, `promise_finally`, `async_resume`, `generators`,
`async_generators`.

Two shape changes were needed rather than a pure move:

- `promises` values became `Rc<RefCell<PromiseData>>`. A reader that held the
  map's `Ref` across a promise hook, a job enqueue or an allocation would either
  abort every collection (the `RefCell` trace path) or panic when the hook
  inserted a promise into the same map. Cloning the `Rc` out of a short-lived
  borrow removes both hazards; `Agent::promise_data` is the one place that does
  it.
- Reads of a table write `let rec = agent.promise_data(id);` or
  `.borrow().get(..).cloned()` rather than `if let Some(x) = map.get(..)`, which
  would extend the guard over the body. `if let` / `match` on a borrowed
  `VecDeque` in the drain loop was likewise split into a binding followed by the
  test.

Measured on release builds, old binary vs new, interleaved on the same machine
(median of 3-7 runs): a 50 000-link settled-promise chain is 20.5 s -> 21.2 s
(~3%, the extra `Rc` allocation and the pointer chase in the traced table);
50 000 four-step generator drains are 271 ms -> 276 ms (within noise). The
promise path is dominated by something unrelated to this table (a settled chain
costs ~400 us per link), so the delta is the table's own cost, not a general
slowdown.

Gate run: `cargo test -p runtime --release --lib` (1068 pass), clippy clean,
`built-ins --filter "*Promise*"` 732/732 and `*Generator*` 157/157 pass, plus
`--gc-stress --gc-verify` samples over `Promise/reject`, `GeneratorPrototype/
return`, `expressions/await` and an async/generator/combinator script.

### Option B landed: the callback agent is explicit

`crux::function::NativeFn` and `NativeCtor` now take an `AgentPtr` (an opaque
`*mut ()`) as their first argument, and the dispatching frame supplies its own
live `&mut Agent` reborrow: `runtime::function::call_inner` and
`construct_inner`, the VM's `PendingCall`/`CallBuiltin` native arms, the JIT's
native helper, and the earlier `fast_call` lane. A body that needs the agent
recovers it with `context::callback_agent(ptr)`, which is sound because the
pointer's provenance is the frame that is still running — unlike the old
`with_agent` TLS slot, which could hold an ancestor of the live `&mut Agent`s
and whose retag popped their borrow tags.

Consequences: `context::current_agent_mut` is gone (nothing derives `&mut Agent`
from TLS any more); `embed::FunctionCall` carries the pointer and its methods
(`to_number`, `to_string`, `call`, `construct`, `eval`, `create_object`,
`create_array`, `create_function`, `define_accessor`) run on it; the CLI's
globals (`parseInt`, `isFinite`, `escape`, `queueMicrotask`, console,
`setTimeout`/`setInterval`) and test262's `$262.gc`/`createRealm`/`evalScript`
hooks take the same route. `crux::function::current_agent` remains for reads
(the allocation-time collector, the JIT's callee lookup) and for crux's own
re-entry into an ECMAScript body from a proxy trap or a `ToPrimitive`
conversion, which still goes through the `EcmaHook`; threading the agent
through those crux entry points is separate, larger work, and the collector
side is what slices 2-4 address.

Gate run: `cargo test --workspace --release --no-fail-fast` (every target green
except the two pre-existing `v8` snapshot `should_panic` tests, which fail
identically at the commit before this change); clippy `-D warnings` clean;
`*Promise*` 732/732, `Function/*` 509/509, `WeakRef/*` 29/29,
`FinalizationRegistry/*` 47/47, `expressions/await/*` 22/22; CLI host-globals
and async/generator scratch scripts under `--gc-stress --gc-verify`.

### Slice 2 landed: the realm tables, and a roots pointer in the registry

Moved to `AgentRoots`: `realms`, `function_realms`, `shadow_realms`,
`shadow_wrapped`, `shadow_handlers`. The three ShadowRealm tables were plain
`IdMap`s and became `RefCell`s; every read that keeps the guard over work that
may allocate now takes what it needs out of a scoped borrow (the import
continuation copies its export name out, the wrapped-function lookup copies its
target and realm), for the same reason slice 1's promise reads do.

`LIVE_AGENTS` entries are now `(signifier, agent, roots)`, and `collect_with`
traces every registered agent's moved containers through its `*const AgentRoots`
— never through `&Agent` — while the not-yet-moved state is traced separately by
`Agent::trace_agent_state` (renamed from `trace_roots`). That split is what
makes the remaining slices mechanical.

A roots-only registry is deliberately not done yet: other agents' un-moved state
(still most of `trace_agent_state`) would stop being traced, and a nested or
sibling agent's only reference to a buffer or data table is that state, so the
next collection would free it. The agent pointer leaves the registry when
`trace_agent_state` is empty, i.e. at the end of slice 4.

Gate run: clippy clean, `cargo test -p runtime --release --lib` 1068 pass,
`ShadowRealm/*` 64/64 under `--gc-stress --gc-verify`, `Promise/*` 732/732 under
`--gc-stress --gc-verify`, `language --filter "*realm*"` 19/19.

---

## What was checked and found sound

- **No `unsafe`** in `lexer`, `parser`, `syntax`, `regexp`, or `unicode`; span/slice
  clamping in `SourceText` is correct.
- **Module loader is host-map-only** (`runtime/src/module.rs:710`): imports resolve
  out of `Agent::host_modules`; `..`, absolute paths, drive letters, UNC,
  `file://`, and `data:` cannot reach the filesystem. Dynamic import fails closed.
- **WASM effective-address arithmetic** is sound: interpreter `mem_start` and
  compiled `mem_ea` use `checked_add` plus bounds checks; SIMD lane indices are
  validated; `call_indirect` compares rec-group-equivalent signatures.
- **JIT helper ABI**: all 148 helper fields match names/arity, `JitCallContext` is
  `repr(C)`, calls use the platform C ABI, and `extern "C"` helpers abort rather
  than unwind into machine frames.
- **FFI marshaling**: UTF-16 is lossless, UTF-8 goes through `to_string_lossy`, id
  tables are monotonic and double-release is idempotent.
- **NaN-boxing invariants** hold; `encoded_box_address` is only used as a
  conservative root filter.
- **Dependencies**: `cargo audit` reports **0 vulnerabilities** across 134 packages;
  the direct set is small and tightly pinned (Cranelift `=0.134.3`,
  `libc 0.2.189`, `num-bigint 0.4.8`, `ryu 1.0.23`).
- **Tooling**: `clippy -- -D warnings`, `--gc-verify`, `--gc-stress`,
  `--nursery-stress`, and a conformance sweep are in place. The GC verifier is
  genuinely effective — it caught C-1.

---

## Prioritized remediation

1. **Fix C-1 first.** Add the missing `max_stack_usage` arms, assert
   `sp ≤ buf_end`, add the barrier in `array_extend_dense`, and add a
   `--gc-stress` smoke test to CI. This is the only reproduced memory-safety bug
   in the default build.
2. **Harden the JSC C API** (refcounts, per-isolate pins, stable string storage,
   accumulator registry) before shipping `--features jsc` to any host.
3. **Bound recursion and allocation** from JS input: parser/JSON/proxy/flat depth
   budgets, `try_reserve` for `ToLength`, BigInt bit cap, WASM page/table caps,
   RegExp step budget.
4. **Eliminate panic paths**: the sort total-order panic, the Intl month-13 index,
   and the `with_capacity` aborts are all one-line fixes; then make `ffi::guard`
   poison the context.
5. **Make TLS state RAII** (`ACTIVE_RUNS`, `CURRENT`, `LIVE_AGENTS`,
   `CURRENT_AGENT`) so caught panics cannot leave stale raw pointers.
6. **Capability model:** `--allow-fs[=ROOT]` for the CLI, an asset root for raylib,
   and `create_new` for temp files.
7. **Supply chain:** pin actions to SHAs, run `cargo audit` in CI, build Deno with
   `--locked`, add `publish = false` to `crates/v8`, and validate/escape
   build-script identifiers.

---

## Suggested `.rules` additions

- `max_stack_usage` must mirror the interpreter's push/pop for every `Step`; a new
  pushing step must be added there and to `emit_step` in the same change.
- Never size a `Vec` from `ToLength`/a JS-controlled length; use growth or
  `try_reserve` mapped to a JS error.
- Every recursion over untrusted input needs an explicit depth budget; a Rust
  stack overflow aborts the host, and `catch_unwind` cannot recover it.
- Any TLS state pushed around a fallible call must be popped by a `Drop` guard;
  panic containment exists (`ffi::guard`, sweep), so stale entries outlive the call.
- A container `trace_roots` reads belongs in `AgentRoots`, behind `RefCell`, and is
  reached through `&self`; adding a traced table as a plain `Agent` field
  reintroduces the M-1 aliasing that slice 1 removed.
- When a table's own `RefCell` guard would be held across a host hook, an
  allocation or a call that may mutate the table, clone the `Rc`-boxed record out
  of a short borrow instead of binding the guard (`if let Some(x) = cell.borrow()
  .get(k)` extends it over the body).
