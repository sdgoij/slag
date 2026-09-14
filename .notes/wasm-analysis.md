# WebAssembly engine: current-state analysis (interpreter + Cranelift compiler)

Scope: `crates/wasm` (decode, validate, execute, compile), the JS-API layer
(`crates/runtime/src/builtins/wasm.rs`), and the conformance harness
(`crates/wasmtest`). Written 2026-09-14.

Method: four deep read-only passes (interpreter, Cranelift backend,
decoder/validator, JS-API + harness), then spot verification of the
highest-impact claims. Every claim carries `file:line`; the ones marked
**(verified)** were re-read directly while writing this. No sweeps were run
while writing this; §3 now records the 2026-09-14 sweeps.

Fix status (2026-09-14): §6.2.1-6.2.3 are landed in
`crates/wasm/src/binary.rs` (commit `d7ee662`) with four regression tests. The
second half of §6.2.3 ("missing features reported as malformed") was
deliberately left unchanged, and §6.2.4 remains open. The §3 sweeps pass with
0 fail and 0 pendings, and the core total matches the documented 64,594 to the
test — so **the corpus does not exercise any of the three fixes**. They are
hardening changes (a reserved encoding or an oversized body can no longer be
misclassified as pending, and an untrusted field count can no longer
allocate), not a pass-count improvement: the pending tally was already 0
before them. The `file:line` anchors in §6.2 are pre-fix and drift by the ~86
lines that change added. Separately, §3's JS-API failure — a real gap, not a
§6.2 item — is fixed in `builtins/wasm.rs::shared_memory_buffer`.

## 1. Where it stands

| Area | State |
|---|---|
| Decoder / validator | Complete for the decoded proposal set; a full spec validator (§3) |
| Interpreter | The shipped path; correct against the corpus, and the slow path (§4) |
| Cranelift backend ("Cut 11") | Written, equivalence-gated, **on by default for native builds, interpreter-only on wasm32** (§5) |
| JS-API layer | Complete except `WebAssembly.Function` and the streaming helpers (§3, §6) |
| Threads / atomics | **Not implemented at all** (§6.1) |
| Conformance | **Reproduced 2026-09-14:** 64,594 core / 0 fail / 0 pending; JS-API 1,001 / 0 (§3) |

## 2. Architecture

Pipeline: `binary.rs` (decode) → `valid.rs` (validate) → `exec.rs`
(instantiate + execute), with `compile.rs` as an optional Cranelift second
execution path behind the `compile` feature.

- `Store` owns all instances and shared cell pools (globals, memories, tables,
  tags, exceptions, GC objects, host funcs) plus parked runs (`exec.rs:324-357`).
  `Instance` holds the module and per-space cell-id vectors, so an import
  aliases the exporter's cell (`exec.rs:299-321`).
- `Engine` owns one shared operand stack (`Vec<Value>`) and an explicit frame
  stack; calls push frames rather than recursing on the host stack, and tail
  calls replace the top frame (`exec.rs:5894-6074`). Depth is bounded by
  `DEFAULT_DEPTH_LIMIT = 4096` per instance (`exec.rs:110`, **(verified)**).
- The host boundary is a suspend/resume protocol: a call into a host function
  parks the whole stack+frames into `suspended` and returns `Ctl::Host`
  (`exec.rs:4071-4077`, `3420-3454`); the JS-API layer resolves the token and
  resumes.
- Wasm exceptions allocate an `ExceptionInst` in the store and unwind by
  scanning the top frame's live `try_table` scopes (`exec.rs:4088-4207`).
- The JS-API layer keeps a `wasm_store` plus a dozen `HashMap` registries on
  the runtime `Agent`, traced as GC roots (`runtime/src/agent.rs:764-861`).

## 3. Coverage

Supported (decoded *and* executed): MVP, multi-value, sign-extension,
saturating conversions, reference types / typed `funcref`, bulk memory, tail
calls, the new exception handling proposal, GC (structs, arrays, i31, casts),
the full SIMD `0xfd` space, relaxed SIMD, memory64, multi-memory, table64.

Not implemented (whole features):

- **Threads / atomics.** `0xfe` is rejected as *malformed* (`binary.rs:1064-1077`),
  the shared-limits flag as malformed (`binary.rs:553`), and shared-everything
  types as `Unsupported("shared type")` (`binary.rs:392`). No atomic op exists
  in `instr.rs`. (Shared *memory* itself works at the JS-API level — Cut 10
  wave 5 — but nothing can issue an atomic access.)
- **Stack switching / continuations** — `Unsupported("continuation type")`
  (`binary.rs:420`).
- **`WebAssembly.Function`** (typed function wrappers) — absent, so raw JS
  closures cannot be funcref table elements (`runtime/.../wasm.rs:2252-2260`,
  `2505-2510`).
- **`compileStreaming` / `instantiateStreaming`** — deliberately not installed
  (`.notes/wasm-plan.md:820-822`).

**Conformance, reproduced 2026-09-14.** With the submodule initialised
(`waspec` @ `37d6b0591`) both sweeps reproduce every per-suite figure in
`README.md:237-245`, and 0 pendings holds:

| Suite | Pass / fail / pending |
|---|---|
| baseline `core/*.wast` | 20,662 / 0 / 0 (3 skipped) |
| proposals (simd, relaxed-simd, bulk-memory, exceptions, gc, memory64, multi-memory) | 43,932 / 0 / 0 (1 skipped) |
| **core total** | **64,594 / 0 / 0** |
| JS-API (`wasmtest jsapi`) | 1,001 / 0 / — (16 skipped) |

The core total matches the documented figure to the test, so the earlier
"not reproducible here" caveat was purely the uninitialised submodule. The four
skips are the `wasm-exclusions.txt` taxonomy entries (three baseline harness /
module-linking / lexer files, plus `gc/type-subtyping.wast`); the one runner
wart that remains is that duplicate `.wast` basenames share a cache key, so a
combined `core` + `multi-memory` invocation refuses to run (`memory_grow.wast`
exists in both) and the suites must be swept separately.

The JS-API row first measured 1,000 pass / **1 fail**: `memory/grow.any.js`
failed `Growing shared memory does not detach old buffer` on
`assert_equals(Object.isFrozen(actual), shared)`
(`js-api/memory/assertions.js:26`). That was a real gap, now fixed. The cause
was scope, not a missing rule: `array_buffer::shared_array_buffer_from_block`
never sealed its result, and `builtins/wasm.rs`'s own test even pinned the wrong
behaviour (`if (Object.isFrozen(sab) || !Object.isExtensible(sab)) return false`
— contradicting the comment directly above it). The wasm JS-API path now seals
its buffer in `shared_memory_buffer`, while `new SharedArrayBuffer(...)` and the
worker path stay extensible, which is exactly what V8 does:

```
node probes (v24.12.0):
new SharedArrayBuffer(8)                        -> frozen false, extensible true
new WebAssembly.Memory({shared:true}).buffer    -> frozen true,  extensible false
new WebAssembly.Memory({}).buffer               -> frozen false, extensible true
```

Sealing globally would also have broken the pinned test262 SAB species fixtures
(`test/built-ins/SharedArrayBuffer/prototype/slice/species-*.js` assign
`constructor` on an instance), which is presumably why an earlier attempt was
backed out — so the seal is scoped to the wasm path and pinned by
`wasm::tests::shared_memory_buffers_are_sealed_but_plain_sabs_are_not`. Re-verified
for the fix: JS-API 1,001 / 0, `built-ins` SAB 104/104, ArrayBuffer 221/221,
Atomics 389/389, `cargo test -p runtime --lib` 759 pass, workspace clippy clean.

## 4. The interpreter (`exec.rs`)

Design is sound: a flat `step` loop, no host-stack recursion, explicit frames,
correct tail-call frame replacement, and a real unwinding path for exceptions.

Per-instruction costs that matter (all **(verified)**):

1. **Every instruction is cloned.** `let instr = self.body(frame_index)[pc].clone();`
   (`exec.rs:4216`) — a 32-byte copy per step, plus a heap allocation for the
   `Vec`-carrying variants (`BrTable`, `SelectTyped`, `TryTable`). `enter_structured`
   clones the same instruction again (`exec.rs:5647`) and `catch_in_top_frame`
   clones the `TryTable` again (`exec.rs:4123`).
   **Partly fixed 2026-09-14:** both double clones are gone — `enter_structured`
   takes the instruction `step` already owns, and `catch_in_top_frame` decides
   its clause under an immutable borrow instead of cloning the `TryTable` and
   building a per-clause cell `Vec`. The per-step clone itself remains: see
   §7's landing note.
2. **The body map is rebuilt per branch.** `map_for` calls `precompute`, which
   allocates two `Vec`s sized to the whole body and rescans it (`exec.rs:5746`,
   `3936-3959`) — on every `br`/`br_if`/`br_table`, `if`, `else`, and catch
   dispatch. **Fixed 2026-09-14:** computed once per body and cached on the
   instance (`Instance::body_maps`, filled lazily on the first branch into the
   body).
3. **`Instr::Num` allocates a `Vec<Value>` per numeric instruction**
   (`exec.rs:4357-4366`). **Fixed 2026-09-14:** a stack array (the arity is at
   most 2).
4. **Branches and returns allocate.** `take_top` does
   `self.stack[start..].to_vec()` (`exec.rs:6113`), and `finish_top` likewise
   (`exec.rs:6096`). **Fixed 2026-09-14:** `move_top_to` closes the gap in
   place with `copy_within` + `truncate`; only the outermost frame's results
   are still allocated, once per invocation.
5. **`Value` is 24 bytes** (`values.rs:261-268`). It was 32: `V128(u128)` forced
   16-byte alignment. **Fixed 2026-09-14:** `V128` is now `[u64; 2]` (the compiled
   path's lo/hi word pair), 8-byte aligned. 16 needs the v128 payload ≤8 bytes —
   an indirection, and both suggested mechanisms are unsound here: `Box<u128>`
   breaks `Value: Copy` (relied on by the `seed_globals_into`/`ref_to_token`
   matches), and a side v128 stack dangles because v128 values escape into
   store-lifetime containers (global cells, struct/array cells, results). 24 is
   the inline floor.
6. **Call setup allocates**: a declared-locals `Vec<ValType>` clone, an args
   `Vec`, and a one-element `labels` `Vec` per call (`exec.rs:5938-5959`).
   **Fixed 2026-09-14:** the clone is gone (the body's locals are re-borrowed,
   not copied), and popped frames' `locals`/`labels` buffers recycle through
   per-invocation pools instead of being reallocated per call.
7. **Memory access resolves the cell twice and re-checks twice**: `pop_mem_addr`
   → `memory_is64` → `mem_cell`, then the access resolves again
   (`exec.rs:3988-4012`, `4487-4504`), with two `checked_add`s per access even
   though `offset`/`size` are compile-time constants.
   **Fixed 2026-09-14:** `pop_mem_addr_cell` resolves the cell and pops the
   address in one pass, and `mem_start` collapses the two adds into one. It is
   a code simplification, not a measured win (see §7's landing note).

`.notes/wasm-depth.md:46-49` measures the consequence: **≈60-100× slower than
native per wasm interpretation layer**, which is why the nested-engine
experiment (native → Slag-in-wasm → module) boots in ~11 s.

## 5. The Cranelift backend (`compile.rs`, feature `compile`)

Architecture: one compiled entry per module-defined body, parallel to
`Module::bodies`; the interpreter remains the oracle and the fallback. Entry
ABI is `extern "C" fn(args, nargs, mems, ncount, gvals, out, nout, store,
instance, grow, call, scratch) -> i32` where every slot is a 64-bit word and a
returned `i32` is a trap code (`compile.rs:62-70`, `214-227`).

Policy and gaps:

- **Lazy since 2026-09-14 (item 14).** `instantiate` compiles nothing; a body
  compiles the first time execution reaches it (`Store::ensure_compiled`),
  which installs the entry in `Instance.compiled` and in the instance's
  direct-call table. Until then it runs interpreted — every compiled call site
  already treats a zero entry as "not compiled" and falls back, so that state
  needed no new plumbing. The call helper's native re-entry compiles on demand
  too, so a callee the tier has not reached still ends up native there (that
  path is depth-counted, so the native stack bound is unchanged). `compiled`
  is written only there; nothing recompiles and nothing invalidates.
  Measured on `fixtures/many-bodies.wast` (1,001 bodies of 24 ops, of which
  only two are reached): **0.25 s → 0.054 s**, level with the interpreter's
  0.053 s — the ~0.2 s of eager per-body compile cost is gone. The coverage
  report stays a *static* measure, so `Store::compile_coverage` forces every
  body through the tier; `wasmtest run --compiled` no longer asks for it
  (`equiv`, which prints it, does), which is what let the probe path measure
  the tier at all instead of a hidden eager compile.
- **Host-reachable bodies are forced interpreted** (`exec.rs:3117-3152`,
  **(verified)**): a native run cannot park at an external-host boundary, so
  one host-call mechanism is kept (plan decision 7).
- **Calls go through a store-side helper.** Direct, indirect, `call_ref`, and
  tail forms spill their arguments into a scratch region and call the helper
  (`compile.rs:3069-3187`, `exec.rs:492-755`), which resolves the target and —
  within `NATIVE_CALL_DEPTH = 64` (`exec.rs:119`) — re-enters the callee's
  compiled entry natively. (The earlier note here said "calls are never
  native"; that misread the helper, which does re-enter — it just does the
  resolution and the per-call buffer setup in Rust first.) Past the cap, or for
  an interpreted or host callee, the helper runs the callee through the
  interpreter instead.
  **Fixed 2026-09-14, in three slices.** (1) The per-call buffers — memory
  descriptors, the used-global cells and `gvals`, and a 2 KiB `SCRATCH_SLOTS`
  scratch, five `Vec`s per call — are cached per native depth on the store and
  reused (`Store::native_frames`). (2) The callee's declared type is resolved by
  reference (`Store::func_type_ref`, with `func_type` now a one-line wrapper
  over it), so only the interpreted fallback materializes a `FuncType`, which
  had been two more `Vec`s per call. (3) A `call`/`return_call` to a defined
  function of the caller's own module now calls it **directly**: the instance's
  compiled-entry table rides the scratch region's metadata slot
  (`compile::ENTRIES_SLOT`), the callee's entry is loaded from it and invoked
  with the same ABI, and a zero entry — the callee stayed interpreted, or the
  store forced the interpreter — falls back to the helper, byte-for-byte the
  old path. The caller's scratch is shared, which is safe because the entry
  prologue reads its arguments out before any call site reuses those slots.

  Two conditions gate the direct form, both derived from predicates the
  compiler already computes rather than from a maintained allowlist: the callee
  must need no `gvals` buffer (the caller lends its scratch but not its global
  slots — the same `body_has_calls`/`body_globals` pair `Engine::compile` sizes
  the buffer by), and it must make **no call at all**. The second is the
  native-stack bound: a direct call adds a frame with no depth accounting in the
  shared scratch, so only a body that cannot recurse may be reached that way;
  call-bearing callees keep going through the helper, whose `NATIVE_CALL_DEPTH`
  budget is what bounds the stack. The corpus caught exactly that: the first cut
  allowed call-bearing callees and core root overflowed the native stack
  (`thread 'main' has overflowed its stack`) — the suites without deep recursion
  were all green, which is why the whole-corpus run is the gate.

  Together the three slices took the call-per-iteration probe from 2.6× to
  **14.3×**, leaving it at the call-free leaf's own floor.
- **Helpers, not native lowering**, for: all calls, `memory.grow`, imported /
  call-bearing-body globals, table ops, GC allocation/casts, EH dynamic
  dispatch, and the SIMD forms outside the native subset (`compile.rs:4817-4887`,
  `exec.rs:505-565`). **Partly fixed 2026-09-14:** plain `v128.load` and the
  pure-register SIMD ops that map one-to-one onto Cranelift vector/bitwise
  instructions (`v128.not`, bitwise `and`/`andnot`/`or`/`xor`, the integer
  `*.add`/`*.sub` family, and `*.extract_lane`) lower natively; everything else
  keeps the helper.
- **Bounds checks are emitted per access.** The memory descriptor (ptr+len) was
  reloaded every time, because `memory.grow` reallocates the backing store.
  **Fixed 2026-09-14:** the descriptor now rides a cached `Variable` pair seeded
  in the entry block, re-read only after a call or `memory.grow`
  (`Lowerer::mem_vars` / `reload_mem_vars`). The per-access bounds check itself
  remains — its address is loop-varying, so it is not hoistable without range
  analysis.
- **Caps:** params+results ≤ `SCRATCH_SLOTS = 256` u64 words
  (`compile.rs:996-1005`), ratified as out of scope (plan decision 6).

**Shipped and on by default for native builds (2026-09-14).** `runtime`
activates `wasm/compile` from a `cfg(not(target_arch = "wasm32"))` dependency
table, so `cli`, `slag`, and every native embed get the compiled path without a
feature flag. It is **native-only by construction**: cranelift's `region`
dependency (executable-page allocation) has no wasm32 backend, so the `wasm`
crate scopes those dependencies off wasm32 and rejects the combination with a
`compile_error!` naming the reason — a wasm embed always runs the interpreter,
exactly like the JIT (`jit` fails the same way, in the same crate). The target
table is what keeps that honest without changing any build command: the
documented `--target wasm32-unknown-unknown` example build never sees the
feature, so the browser demo is untouched. There is deliberately **no
build-time opt-out on native** (a target table cannot be feature-gated) —
`Store::set_compile(false)` is the per-store way back, and the `wasm-compile`
feature remains an explicit request (on wasm32, a hard error). A marker feature
that the target table keys off, so a native embed could drop cranelift, was
considered and declined (2026-09-14): the compiled path is the native default,
and the per-store switch covers the only case that needs it. `crates/wasm`
still defaults the feature off at the crate level, because the crate cannot
express "native only" by itself.

What it buys, measured (release; the compiled column includes ~20 ms of process
startup, so the leaf row understates the loop):

| Workload | Interpreter | Compiled | |
|---|---|---|---|
| call-free leaf loop, 10M iterations (`fixtures/leaf-loop.wast`) | 2.044 s | 0.026 s | ~80× |
| loop with one call per iteration, 1M (`fixtures/interp-hot-loop.wast`) | 0.324 s | 0.023 s | ~14× |
| store + load per iteration, 2M (`fixtures/mem-loop.wast`) | 0.772 s | 0.021 s | ~37× |
| two register v128 ops per iteration, 2M (`fixtures/simd-loop.wast`) | 0.416 s | 0.023 s | ~18× |
| `array.set` + `array.get` per iteration, 2M (`fixtures/gc-loop.wast`) | 0.302 s | 0.034 s | ~8.9× |

All are medians of three runs of the committed probes, which isolate execution
(the compiled column still includes ~20 ms of process startup) and whose
expected values are V8-verified. Whole-suite timings are **not** a usable
per-path measure: a suite run is dominated by converting, decoding, validating,
and instantiating, and the single-run figures previously quoted here (1.42× on
`bulk-memory`, 0.93× on `gc`) did not reproduce — re-measured, both land inside
the run-to-run noise, with the interpreter's own number moving as much as the
difference. What the whole corpus does establish is correctness, and there both
paths report identical totals (§3).

The memory and GC rows are what settled the default-on question. The earlier
reading here was that helper round-trips might leave memory- and vector-heavy
bodies at a wash against the interpreter, which would have made item 6 and item
10 blockers. They do not: the compiled access is a few cycles against the
interpreter's heavy per-access cost, and the two regimes where that could have
gone the other way are ~37× (memory) and ~8.9× (GC aggregates) ahead. Register
SIMD is the narrowest margin at ~4.8×, where item 10's helper round-trip is
indeed most of the compiled time — a further win, not a correction.

Verified for the landing: `equiv` over the control-flow files, `exceptions`,
`bulk-memory`, and `gc` reports **36 suites, 0 diverged**, 1,986/1,986 module
definitions compiled; the corpus and JS-API totals are unchanged with the
compiled path as the native default (§3).
- **Shipped and default-on for native:** `runtime` activates `wasm/compile`
  from a `cfg(not(target_arch = "wasm32"))` table (above), so `cli`, `slag`, and
  native embeds run the compiled path without a flag. The wasm32 browser build
  is unchanged, and `crates/wasm` still defaults the feature off at the crate
  level because it cannot express "native only" on its own (confirmed:
  `cargo tree -p slag --target wasm32-unknown-unknown -e features` shows no
  cranelift) **(verified)**.
- **Doc drift (fixed 2026-09-14):** the module doc claimed "GC objects are not
  lowered yet" and 32-bit-only tables, and `body_compile_reason` named a
  `try_table` gate that `lowerable` no longer has. All three are corrected: the
  header now covers the GC aggregates and exception handling, says table
  addressing may be 32- or 64-bit, and no longer calls the subset "leaf
  functions" (calls lower); the gate list names only the structural gate, a
  non-carried parameter/result/local type.

## 6. Gaps

### 6.1 Spec features

Threads/atomics and stack switching (§3) are the two whole proposals missing.
`WebAssembly.Function` is the one JS-API interface missing.

### 6.2 Correctness and robustness defects (actionable)

1. **Function-body trailing bytes are accepted.** `decode_body` decodes the
   locals and the expression, then discards `pos` — it never checks that the
   expression consumed the declared body size (`binary.rs:838-859`
   **(verified)**), so `0b 41 00 0b <garbage>` decodes with the garbage
   silently ignored, even though the section-level equivalent is rejected
   (`binary.rs:339-341`). A real decoder-strictness bug.
   **Fixed 2026-09-14:** `decode_body` now rejects a body whose expression does
   not end exactly at the declared size (`Malformed("trailing bytes in function
   body")`).
2. **Untrusted pre-allocation.** `decode_comptype`'s struct case does
   `Vec::with_capacity(count as usize)` from an attacker-controlled field
   count (`binary.rs:413-414` **(verified)**) — a ~2^32 count attempts a
   multi-GB allocation before any bounds check. The same file explicitly caps
   the analogous local-group expansion against hostile counts
   (`MAX_LOCALS`, `binary.rs:845-852`), so the struct path is inconsistent with
   the decoder's own policy.
   **Fixed 2026-09-14:** the struct case now pushes incrementally, matching
   `read_valtype_vec`'s documented no-preallocation policy.
3. **`Unsupported`/`Malformed` misclassification.** The harness maps
   `Unsupported` → *pending* and anything else → *fail*
   (`wasmtest/src/main.rs:581-590`), so classification is load-bearing.
   Undefined encodings reported as *unsupported* (should be malformed):
   data-segment flags (`binary.rs:834`), unknown `0xfc`/`0xfb`/`0xfd`
   subopcodes (`binary.rs:1241`, `1334`, `1113`, `1194`). Missing features
   reported as *malformed* (should be unsupported): the atomics prefix
   (`binary.rs:1076`) and the shared-memory flag (`binary.rs:553`).
   **Fixed 2026-09-14 (first half only):** the undefined encodings above are now
   `Malformed`, as is the dead `instruction` fallback (`binary.rs:1081`). The
   second half was deliberately *not* changed: under the pinned spec there is no
   threads proposal, `Limits::shared` is never populated, and `0xfe` is already
   `Malformed` for the same stated reason, so the atomics prefix and the
   shared-memory flag stay `Malformed`. Revisit only if a threads cut lands.
   The corpus does not exercise the flipped arms — the core total is 64,594 / 0
   with 0 pendings, unchanged (§3) — so this is hardening against inputs the
   corpus lacks, not a counted fix.
4. **The validator's `Unsupported` path is dead.** `valid::Error::Unsupported`
   (`valid.rs:28-31`) is never constructed, so any validator gap surfaces as a
   *failure* rather than *pending* — the honesty mechanism the crate documents
   is inert in phase 2. **Deferred:** out of scope for the 2026-09-14 decoder
   cleanup; wiring it moves validator verdicts corpus-wide and needs its own
   sweep.
5. **Supertype well-formedness was under-validated.** `build_spaces` bounded the
   supertype index but checked neither composite-kind compatibility, supertype
   finality, nor field/parameter variance (`valid.rs:89-108`), and
   `decode_subtype` rejected a supertype **count** above 1 as
   `Malformed("malformed subtype")` — a *validation* rule (spec K-sub's
   `|x*| <= 1`) enforced in the decoder, which would have turned the corpus's
   `assert_invalid` for that case into a failure. **Fixed 2026-09-14, both
   halves.** The decoder now accepts the list (the binary grammar bounds it not
   at all: `x*:Blist(Btypeidx)`, spec 5.3) and pushes incrementally rather than
   preallocating from the untrusted count, and `valid` enforces all four K-sub
   conditions: at most one supertype, a supertype that strictly precedes the
   subtype (the old group-end bound admitted a *later* member of the same rec
   group, and `x < x_0` is also what keeps the supertype walk acyclic), a
   non-final supertype, and `Comptype_sub` — kind agreement, a struct's fields
   as a prefix, constant fields covariant, mutable fields invariant, and
   function parameters contravariant with results covariant, over a new
   `heap_matches` that covers the abstract lattice, the bottom types, and the
   declared supertype chains.
   **The corpus file stays excluded.** `gc/type-subtyping.wast` cannot be
   swept: its `multiple supertypes` case is `(sub $a $b …)` *text*, and the
   pinned `wast` 258.0.0 grammar rejects a second type index (the spec's own
   text rule is `x*:Tlist(Ttypeidx)`, so this is a tooling lag, not a spec
   limit; the local registry has no newer `wast`, and wabt is not on PATH).
   `crates/wasmtest/fixtures/type-subtyping.wast` therefore pins the same rules
   in a parseable form — the multi-supertype case encoded as bytes — and
   `tests/type_subtyping.rs` gates it. Every module in it is cross-checked
   against V8 (node v24.12.0), which accepts the valid module and rejects all
   ten `assert_invalid` cases.
6. **Tokenless host functions cannot return results.** A *type-only* host
   function with a non-empty result type is `Unsupported("host function
   results")` from the top frame and in tail position (`exec.rs:5880-5888`,
   `6012-6013` **(verified)**). Real (token'd) host functions are unaffected;
   this is the `spectest`-stub path only.
7. **A dropped return value.** `write_memory`'s `bool` (a length-mismatch
   signal) is discarded (`runtime/.../wasm.rs:1457`) — a silent no-op if the
   invariant ever breaks.

### 6.3 Operational and harness honesty

- `wasmtest run` exits non-zero on `fail`, and on `pending` once `--strict` is
  passed — which is what the documented sweeps pass, so "0 pendings" is
  enforced by the gate rather than by documentation. A `skip` is a written
  taxonomy entry in `wasm-exclusions.txt` and never fails either way.
  **Fixed 2026-09-14:** `--strict` added; all 16 documented sweeps (eight suites
  × both paths) exit 0 under it. `equiv` (the compile-vs-interpreter equivalence
  gate) and the coverage report are still not part of the default sweep; the
  runner's `compile` feature is on by default, so reaching them needs no flag.
  The decoder's malformed/unsupported boundary — encodings the corpus never
  reaches — is *additionally* pinned by
  `crates/wasmtest/tests/decoder_classification.rs` driving
  `fixtures/decoder-classification.wast`.
- `valid.rs`'s cost is quadratic on hostile input: `push_frame` clones the
  whole `init: Vec<bool>` per structured construct (`valid.rs:693`) — O(n·depth)
  for many locals and deep nesting. Not measured; severity speculative.
- `type_indices_equivalent` has no cycle guard beyond the aligned-rec-group
  assumption; only safe because decoding always populates `rec_groups`
  (`binary.rs:199-201`). Not reachable from bytes; speculative.

### 6.4 Open bug leads (nested execution only)

`.notes/wasm-depth.md:51-77` records two anomalies that appear only when the
engine itself runs interpreted (inside a nested wasm engine) and are **not
reproducible natively**:

1. After create-instance-A → call-A → create-instance-B, B's `i32` exports
   return `undefined` without executing and its `i64` export throws
   (`Cannot read properties of null`) — smells like stale/null store-instance
   state.
2. Sustained depth-2 compute (`fib(24)`, `work(100000,1000)`, …) never
   returned within 15 minutes, while small calls finish in seconds.

Neither has been root-caused. If the engine is ever to be used as a host for
itself, these should be reproduced and fixed first.

## 7. Optimization opportunities

Ranked by expected impact × effort. "I" = interpreter, "C" = Cranelift, "J" =
JS-API boundary, "D" = decode/validate.

| # | Area | Change | Impact | Effort |
|---|---|---|---|---|
| 1 | I | Cache the `BodyMap` per body instead of rescanning per branch (`exec.rs:5746`, `3936`) | Very high | Medium |
| 2 | I | Match a borrowed `&Instr` instead of cloning per step; drop the double clone (`4216`, `5647`, `4123`) | High | Med-High |
| 3 | I | Remove the per-op `Vec<Value>` in `Instr::Num`; give `exec_num` scalar entry points (`4357-4366`) | High | Low |
| 4 | I | Non-allocating `take_top`/`finish_top` via `copy_within` (`6113`, `6096`) | High | Low-Med |
| 5 | I | Shrink `Value` 32→24 bytes: `V128` → `[u64;2]`, 8-byte aligned (`values.rs:266`) | Med | Low-Med |
| 6 | C | CSE/hoist per-access bounds checks (address-dependent; needs range analysis) | Low | High |
| 7 | C | Native direct calls between compiled bodies (or inline the leaf fast path), raising the depth-64 cap (`compile.rs:3127-3131`, `exec.rs:650-683`) | High | High |
| 8 | J | Replace the full-memory memcpy bridge with zero-copy aliasing, syncing on grow/detach (`wasm.rs:1452-1502`, `3158/3174`) | High | High |
| 9 | I | Cut call-setup allocation: cache declared locals per body, reuse frames (`5938-5959`) | Med-High | Medium |
| 10 | C | Lower the remaining SIMD forms natively (float/compare/sat/shift/splat/shuffle/lane loads) | Low-Med | High |
| 11 | J | Register the export wrapper in the O(1) `BUILTIN_HANDLERS` registry (drop a HashMap probe per call) (`wasm.rs:2786-2789`, `function.rs:1699-1702`) | Medium | Low |
| 12 | I | Resolve the memory cell once and collapse the bounds test to one compare (`3988-4012`, `4487-4504`) | Medium | Low-Med |
| 13 | I | Cache the body slice / `&mut Frame` once per step instead of re-indexing (`4212-4216`) | Medium | Medium |
| 14 | C | Lazy/tiered compile instead of eager-at-instantiate (`exec.rs:3143`) | Medium | Medium |
| 15 | J | Avoid per-call `FuncType`/module clones and per-i64 `BigInt` allocation (`wasm.rs:3129-3138`, `1220`, `754-758`) | Medium | Low-Med |
| 16 | D | `Box<[Instr]>` bodies / immediate side-tables; stop the one-element `Vec<Instr>` per element item (`binary.rs:770`, `instr.rs:299-326`) | Medium | Medium |
| 17 | D | Share `init: Vec<bool>` across frames instead of cloning (`valid.rs:693`) | Low-Med | Low |
| 18 | D | `validate_exports` O(n²) duplicate check; `func_type_of` O(index) per `ref.func` (`valid.rs:336-341`, `2039-2053`) | Low | Low |

Items 1-4 are all "remove an allocation or a scan from a per-instruction
path", and can be validated with the existing compiled-vs-interpreted
equivalence harness rather than new tests. Item 7's three slices (2026-09-14)
were the same move one level up, ending in skipping the helper outright for an
eligible direct call: the native call path had been allocating seven `Vec`s per
call (five buffers plus the callee's declared type) and paying two Rust frames
plus an indirect call for every call. The call-per-iteration probe went 2.6× →
7.2× → 14.3× — see §5.

Item 14 landed 2026-09-14 as a lazy tier rather than a hotness threshold:
`instantiate` compiles nothing and `Store::ensure_compiled` compiles a body on
first reach, reusing the null-entry fallback every call site already had. It is
the instantiate half of the same "stop paying for work that is not executed"
move; the probe is `fixtures/many-bodies.wast` (0.25 s → 0.054 s) — see §5.

Item 5 landed 2026-09-14 as the 32→24 slice only: `V128(u128)` became
`V128([u64; 2])` (the compiled path's lo/hi word pair), dropping the enum from
32/align-16 to 24/align-8. The full 16-byte target needs the v128 payload ≤8
bytes — an indirection — and both mechanisms the item proposed are unsound here:
`Box<u128>` breaks `Value: Copy` (the `seed_globals_into`/`ref_to_token` sites
match a `Value` out of a place), and a side v128 stack leaves a dangling index
because v128 values escape into store-lifetime containers (`store.globals`,
`Struct(Vec<Value>)`/`Array(Vec<Value>)` cells, and cross-frame results). 16 is
only reachable by reintroducing per-value cost, so the inline floor is what
landed. Interpreter probes moved ~9-13%: simd-loop 0.47→0.41 s,
interp-hot-loop 0.37→0.33 s, mem-loop 0.87→0.79 s. A `size_of::<Value>()`
assertion pins the layout (the 24-byte form hosts `Value`'s discriminant in
`RefValue`'s spare padding, so a new `RefValue` variant would otherwise grow it
silently).

Item 6 landed 2026-09-14 as its descriptor-load half only. The memory
descriptor (data pointer + byte length) was reloaded from the caller-owned
array on every access; it now rides a cached `Variable` pair seeded once in the
entry block and re-read only after a call or `memory.grow` (`Lowerer::mem_vars`
/ `reload_mem_vars`), so a memory loop's per-iteration descriptor loads are
gone. The per-access bounds check stays: its address is loop-varying, so it is
not hoistable without range analysis, which the compiled path deliberately
does not do. The committed probe cannot show the win — `mem-loop.wast`'s 2M
iterations are ~1-2 ms against ~20 ms of process startup, so the compiled
column reads 0.021 s before and after.

Item 9 landed 2026-09-14. Call setup was doing three heap allocations per
call — a `body.locals.clone()`, the args `Vec` (which becomes the frame's
`locals`), and a one-element `labels` `Vec`. The clone is gone (the declared
locals are re-borrowed, not copied), and popped frames' `locals`/`labels`
buffers now recycle through per-invocation pools on the `Engine`
(`take_locals`/`take_labels`/`recycle_frame`), so a call-heavy loop allocates
nothing after the pool warms. `interp-hot-loop` moved 0.33 -> 0.29 s (~11%);
`leaf-loop` and `mem-loop` are unchanged.

Item 12 landed 2026-09-14 as a refactor rather than a speedup. The
load/store/vec access path resolved the memory cell twice (`mem_cell` then
`memory_is64` inside `pop_mem_addr`) and did two `checked_add`s (offset, then
size) per access. `pop_mem_addr_cell` now resolves the cell and pops the
address in one pass, and `mem_start` collapses the two adds into one
(`offset + size` is a static constant), so each access is one cell lookup, one
add, and one compare. `mem-loop` is unchanged at 0.79 s — the removed work was
~1% of the access cost, below the probe's noise — so this closes the item as a
code simplification, not a measured win.

Item 10 landed 2026-09-14 as its highest-value slice: the register-SIMD ops
that map one-to-one onto Cranelift vector/bitwise instructions. `v128.not`,
`v128.and`/`andnot`/`or`/`xor`, the integer `*.add`/`*.sub` family, and
`*.extract_lane` now lower natively (bitcast to the lane vector type, one
vector/bitwise op, bitcast back — with an explicit little-endian byte order on
the bitcast, which changing lane count requires), and plain `v128.load` is a
native 16-byte load. The runtime helper stays the fallback for everything else.
`simd-loop`'s compiled column went 0.087 -> 0.023 s, moving the margin from
~4.8× to ~18× — the one item this session that delivered its rating.

**Item 8 measured 2026-09-14, and it is the largest boundary cost by orders of
magnitude.** `WebAssembly.Memory.prototype.buffer` is a *copy*: the JS-API keeps
it in sync by copying the whole buffer into the engine cell before a run and the
whole cell back out after (`wasm.rs:1467-1550`), at six sites that include every
host call in both directions (`3155-3258`). That is ~3 full copies of the linear
memory per JS↔wasm call, ~5.4 ms each on a 16 MiB (256-page) memory — **360×
the call it is attached to** (a 200-call loop: 3 ms with `.buffer` never touched,
1080 ms once it is), and it scales linearly, so a 1 GiB memory would be ~340 ms
per call. Anything that reads a string out of wasm memory makes every later call
ruinous.

The fix is real aliasing, not a cheaper copy: `buffer` must be an ArrayBuffer
whose byte block *is* the cell's storage, which needs the linear memory to live
in the same refcounted block the JS side uses (`crux::typed_array::SharedBuffer`;
`array_buffer_from_block`/`shared_array_buffer_from_block` already wrap one, so
the JS half exists). Two blockers decide the design:

1. `crates/wasm` currently has **no dependencies at all**, and `crux` pulls
   `num-bigint`/`ryu`/`half`/`libc` — so `wasm` → `crux` would tax the
   nested-engine/wasm32 story (`.notes/wasm-depth.md`) for a byte block. A leaf
   `byteblock` crate (no dependencies, one `workers` feature) re-exported by
   `crux` keeps the engine dependency-free, at the cost of a new crate.
2. Under the `workers` feature `SharedBuffer` is an `Arc<[AtomicU64]>`, which is
   right for a *shared* memory (cross-agent atomics) and wrong for an unshared
   one on the engine's hot path — and `SharedBuffer`'s layout is JIT-visible
   (`crates/crux/src/typed_array.rs:195-201`), so whatever moves, the offsets
   must not.

Slices: (1) storage — `Memory.bytes: Vec<u8>` becomes the shared block behind
`memory_bytes`/`write_memory` adapters (48 `.bytes` sites in `exec.rs`), behaviour
unchanged and corpus-verified; (2) alias — materialise `buffer` over that same
block and delete the six copy sites, keeping only the grow/detach reconciliation
(the JS-API's detach-on-unshared-grow rule is unchanged, and `memory.grow` then
moves the live bytes into a fresh block, which is where the one remaining copy
belongs).

**Slice 1a landed 2026-09-14:** the block type now lives in the new
`crates/byteblock` leaf crate (no dependencies, one `workers` feature), and
`crux` re-exports `SharedBuffer`/`BlockState`/`AtomicOp`/`WORKERS` under their
historical paths, so every user — the JIT's `offset_of!` reads included — is
source-compatible. The block's out-of-bounds error is the leaf crate's own type
with a `From` impl in `crux`, so only the six *tail-expression* returns needed
`.map_err(JsError::from)` and every `?` site is untouched.

**Slice 1b landed 2026-09-14:** the engine's storage is now the block.
`Memory.bytes: Vec<u8>` became a `MemoryBytes` handle that derefs to `[u8]`, so
all 48 `.bytes` sites in `exec.rs` are unchanged — the interpreter's loads/stores,
`memory.copy`/`fill`/`init`, the data-segment paths, and the compiled path's
descriptors (`as_ptr()`/`len()` re-read the live base). A handle rather than an
explicit byte-copy API because the slice-level code is what the engine already
wants and a mechanical rewrite of 48 sites was the larger risk: the raw-pointer
invariant (every deref rebuilds from `SharedBuffer::data_ptr()` and
`byte_length()`; no borrow held across a resize) is written down once, on the
type. `Memory::resize` keeps the single-agent block resizing its `Vec` in place
and, under `workers` where the storage is a fixed atomic array, falls back to
replacing the block and copying — the old block stays alive for any view still
holding it, which an unshared grow detaches anyway. The `workers` build takes
design (a): the engine addresses bytes through the raw pointer, sound only while
no other agent observes the block, and that assumption is recorded on
`SharedBuffer::data_ptr`.

**Slice 2 landed 2026-09-14 — this is the one that pays.** `buffer` is now a
view: `materialize_memory_buffer` wraps the cell's own block
(`Store::memory_block`, via `array_buffer_from_block` for an unshared memory and
`shared_array_buffer_from_block` for a shared one) and the six copy sites are
gone — the two per-run flush/refresh passes, the mid-run pair around each host
call, and the post-grow snapshot/copy. Reconciling survives only as
`memory_buffers_reconcile`, which touches a buffer only when its length no longer
matches the memory. Measured on the same probe: **5,225 µs/call → under the
timer's resolution** (200 calls on a 16 MiB memory now complete in <1 ms, against
1,048 ms), and a host call mid-run reads the live memory instead of costing two
more full copies — which the old design needed for correctness, not just speed.

One design point the corpus's JS-API suite caught, worth recording: an unshared
grow must **move the memory to a fresh block**, not resize in place. Detaching
the previous buffer marks the *block* unreachable (that flag is what views and
the JIT's inline stores read), and under aliasing that block *is* the memory — so
resizing in place leaves the next `buffer` born detached (`memory/grow.any.js`
failed 10 assertions with `expected 0 but got undefined`). A shared grow still
resizes in place, which is exactly why a shared memory's block is allocated at
its declared maximum: prior SharedArrayBuffers must keep aliasing it. The one
remaining copy per grow is therefore the JS-API's own detach rule, not a bridge
tax. Both probes are committed: `tools/wasm_memory_bridge.js` (the A/B cost
loop, over a 256-page memory) and `tools/wasm_memory_alias.js` (the semantics),
each reading its module bytes from a `.hex` beside it, so the numbers above stay
reproducible. The alias probe pins read-through and write-through in both
directions, the detach rules (a stale buffer reports length 0 and constructing
over it throws), a JS-side and a wasm-side grow, and the mid-run host read.

**Landed 2026-09-14 (items 1, 3, 4 and item 2's double clones).** Measured on a
hot-loop probe (1M iterations; a call plus a `br_if` plus three numeric ops per
iteration — `crates/wasmtest/fixtures/interp-hot-loop.wast`, timed as
`target/release/wasmtest run crates/wasmtest/fixtures/interp-hot-loop.wast`):
**0.60 s → 0.32 s**
(~1.87×), item 3 + 4 first (0.60 → 0.41) then item 1 (→ 0.32). Item 2's double
clones do not move that loop (it enters no block and raises no exception), so
they are unmeasured but strictly less work.

The remaining half of item 2 — the per-step `Instr` clone — is deliberately
still in place. `step` matches an owned value because its arms call `&mut self`
methods while the instruction is live, so removing the clone needs either an
`Rc`'d module handle held outside `self` or a split between fetch and execute (a
`Copy` decode-op enum). At ~32 bytes per step that is a few percent, and the two
heap-carrying variants (`BrTable`, `SelectTyped`) still clone their `Vec` on
every execution — the targeted follow-up if `br_table`-heavy code matters.

Verified for the landing: core `20,662` + proposals `43,932` = **64,594 pass,
0 fail, 0 pending** and JS-API **1,001 pass / 0 fail** (both unchanged from
before the change); `wasmtest equiv` over `block`/`br`/`br_if`/`br_table`/
`loop`/`if`/`return`/`call`/`exceptions` reports **12 suites, 0 diverged**, with
577/577 functions compiled; `cargo test -p wasm --lib` 40 pass.

## 8. Recommended sequencing

1. **Correctness first, cheaply:** the body-trailing-bytes bug, the untrusted
   `with_capacity`, and the `Unsupported`/`Malformed` classification (6.2.1-3),
   plus wiring `valid::Error::Unsupported` so pending stays honest (6.2.4).
   These affect the trustworthiness of every conformance number.
   **Landed 2026-09-14:** 6.2.1-6.2.3, except 6.2.3's second half (see 6.2).
   6.2.4 is still open.
2. **Interpreter wins 1-4** — the highest impact/effort ratio in the engine,
   verify covered by `wasmtest equiv`.
   **Landed 2026-09-14:** items 1, 3, and 4, plus item 2's double clones (the
   per-step clone remains — see §7). Measured 0.60 s → 0.32 s on the hot-loop
   probe; corpus totals unchanged, `equiv` clean.
3. **Make the compiler shippable or shelve it explicitly:** as it stands Cut 11
   is a large, default-off artifact with no native call linkage, so its
   real-world benefit is limited to call-free leaf bodies. Either gate it on
   (runtime/CLI feature) after adding direct native calls, or record the
   decision that it stays a research path.
   **Landed 2026-09-14 (all three steps):** reachable as `wasm-compile` on
   `runtime`/`slag`/`cli`, native-only with a `compile_error!` guard; §7 item 7
   (direct native calls, including skipping the helper for an eligible leaf
   call) and item 14 (lazy compile) landed next, and the decision was then made
   to turn it **on by default for native targets** via a target-scoped
   dependency table in `runtime`, which leaves the wasm32 browser build
   untouched. Measured envelope and the remaining refinements (§7 items 6 and
   10) are in §5.
4. **JS-API memory bridge** (item 8) — the only boundary cost that was a whole
   linear-memory copy per call once a `buffer` had been materialised.
   **Landed 2026-09-14:** `buffer` is a view over the memory's own block (the
   `byteblock` leaf crate, engine storage, then aliasing), so the copies are
   gone and a mid-run host read is free. See §7 item 8 for the measurement and
   the one design trap (an unshared grow must move to a fresh block).
5. Threads/atomics and `WebAssembly.Function` are the remaining whole-feature
   gaps; both are proposal-sized cuts, not cleanups.

## 9. How to re-run the gates

```sh
git submodule update --init waspec          # required for any wasm sweep
cargo run -p wasmtest -- run --strict waspec/test/core/*.wast
cargo run -p wasmtest -- run --strict waspec/test/core/simd   # and each proposal dir
cargo run -p wasmtest -- jsapi waspec/test/js-api
cargo run -p wasmtest -- run --strict --compiled ...          # compiled path
cargo run -p wasmtest -- equiv ...                   # both paths, command by command
```

`wasmtest`'s `compile` feature is on by default (without it the runner cannot
force the interpreter, so a plain `run` would silently measure the compiled
path). `equiv` is the compile-path gate: it runs each suite through both paths
and compares the per-command verdicts, and its coverage line is the static
measure of how much of a module can compile.

## 10. Suggested `.rules` additions

- "The wasm Cranelift backend (`wasm/compile`) is on by default for native
  builds: `runtime` activates it from a `cfg(not(target_arch = "wasm32"))`
  dependency table, so `cli`/`slag`/native embeds get it and a wasm32 build
  never does. There is no build-time opt-out on native (a target table cannot
  be feature-gated) — `Store::set_compile(false)` is the per-store way back.
  Compilation is lazy (a body compiles on first reach, `Store::ensure_compiled`)
  but `Store::compile_coverage` force-compiles every body, because it reports
  static eligibility rather than what ran: a timing measurement must not ask
  for coverage."
- "`wasmtest run` cannot fail on `pending`; a real gate must check the pending
  count, and `equiv`/`coverage` need `--features compile`."
