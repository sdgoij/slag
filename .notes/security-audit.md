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
  currently returns a non-match; propagating a `RangeError` is a follow-up.
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
- **C-2 (partial)** — context teardown is deferred to the outermost API
  boundary and reference-counted, `JSStringGetCharactersPtr` points into a
  stable boxed string, the property-name accumulator validates its pointer, and
  string/value retains and `JSValueUnprotect` release their entries.

Outstanding from this report: JSC finalizer wiring (the `finalize` callback
still has no caller), the CLI capability model (H-11), the RegExp budget's
`RangeError` propagation (H-2 follow-up), and the remaining Medium/Low items.

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
