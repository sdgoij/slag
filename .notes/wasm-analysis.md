# WebAssembly engine: current-state analysis (interpreter + Cranelift compiler)

Scope: `crates/wasm` (decode, validate, execute, compile), the JS-API layer
(`crates/runtime/src/builtins/wasm.rs`), and the conformance harness
(`crates/wasmtest`). Written 2026-09-14.

Method: four deep read-only passes (interpreter, Cranelift backend,
decoder/validator, JS-API + harness), then spot verification of the
highest-impact claims. Every claim carries `file:line`; the ones marked
**(verified)** were re-read directly while writing this. No sweeps were run —
see the caveat in §3.

Fix status (2026-09-14): §6.2.1-6.2.3 are landed in
`crates/wasm/src/binary.rs` with four regression tests. The second half of
§6.2.3 ("missing features reported as malformed") was deliberately left
unchanged, and §6.2.4 remains open. Still no sweep — the evidence for the
change is `cargo test -p wasm` (40 pass) plus
`wasmtest check docs/slag.wasm` (ok), not conformance counts. The `file:line`
anchors in §6.2 are pre-fix and drift by the ~86 lines this change added.

## 1. Where it stands

| Area | State |
|---|---|
| Decoder / validator | Complete for the decoded proposal set; a full spec validator (§3) |
| Interpreter | The shipped path; correct against the corpus, and the slow path (§4) |
| Cranelift backend ("Cut 11") | Written, equivalence-gated per the plan, **default-off and not shipped** (§5) |
| JS-API layer | Complete except `WebAssembly.Function` and the streaming helpers (§3, §6) |
| Threads / atomics | **Not implemented at all** (§6.1) |
| Conformance | Documented 64,594 core / 0 fail and 1,001 JS-API / 0 fail; not reproducible in this checkout (§3) |

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

**Conformance caveat.** The documented totals (core 64,594 / 0; JS-API
1,001 / 0; 0 pendings) come from `README.md:230-253` and
`.notes/wasm-plan.md`. The `waspec` submodule is **uninitialised in this
checkout** (`git submodule status` → `-37d6b0…`, empty directory), so no
fixture exists on disk and none of those numbers is reproducible here
**(verified)**. Treat them as documentation until the submodule is initialised.

## 4. The interpreter (`exec.rs`)

Design is sound: a flat `step` loop, no host-stack recursion, explicit frames,
correct tail-call frame replacement, and a real unwinding path for exceptions.

Per-instruction costs that matter (all **(verified)**):

1. **Every instruction is cloned.** `let instr = self.body(frame_index)[pc].clone();`
   (`exec.rs:4216`) — a 32-byte copy per step, plus a heap allocation for the
   `Vec`-carrying variants (`BrTable`, `SelectTyped`, `TryTable`). `enter_structured`
   clones the same instruction again (`exec.rs:5647`) and `catch_in_top_frame`
   clones the `TryTable` again (`exec.rs:4123`).
2. **The body map is rebuilt per branch.** `map_for` calls `precompute`, which
   allocates two `Vec`s sized to the whole body and rescans it (`exec.rs:5746`,
   `3936-3959`) — on every `br`/`br_if`/`br_table`, `if`, `else`, and catch
   dispatch.
3. **`Instr::Num` allocates a `Vec<Value>` per numeric instruction**
   (`exec.rs:4357-4366`).
4. **Branches and returns allocate.** `take_top` does
   `self.stack[start..].to_vec()` (`exec.rs:6113`), and `finish_top` likewise
   (`exec.rs:6096`).
5. **`Value` is 32 bytes** because `V128(u128)` forces 16-byte alignment
   (`values.rs:261-268`) — twice the necessary bandwidth for integer code.
6. **Call setup allocates**: a declared-locals `Vec<ValType>` clone, an args
   `Vec`, and a one-element `labels` `Vec` per call (`exec.rs:5938-5959`).
7. **Memory access resolves the cell twice and re-checks twice**: `pop_mem_addr`
   → `memory_is64` → `mem_cell`, then the access resolves again
   (`exec.rs:3988-4012`, `4487-4504`), with two `checked_add`s per access even
   though `offset`/`size` are compile-time constants.

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

- **Eager, once, never invalidated.** `instantiate` compiles every body
  (`exec.rs:3143` **(verified)**); there is no hotness threshold, no lazy
  compile, no recompile, and `Instance.compiled` is only ever written at
  instantiate.
- **Host-reachable bodies are forced interpreted** (`exec.rs:3117-3152`,
  **(verified)**): a native run cannot park at an external-host boundary, so
  one host-call mechanism is kept (plan decision 7).
- **Calls are never native.** Direct, indirect, `call_ref`, and tail forms all
  go through a store-side helper that runs the callee through the interpreter
  (`compile.rs:3069-3187`, `exec.rs:492-755`). Compiled→compiled native
  re-entry is capped at `NATIVE_CALL_DEPTH = 64` (`exec.rs:119`,
  **(verified)**); past it the helper drops to interpreted subtrees.
- **Helpers, not native lowering**, for: all calls, `memory.grow`, imported /
  call-bearing-body globals, table ops, pure-register SIMD and vec
  load/lane/shuffle, GC allocation/casts, and EH dynamic dispatch
  (`compile.rs:4817-4887`, `exec.rs:505-565`).
- **Bounds checks are emitted per access** and the memory descriptor (ptr+len)
  is reloaded every time, because `memory.grow` reallocates the backing `Vec`
  (`compile.rs:1988-2031`, `1540-1541`).
- **Caps:** params+results ≤ `SCRATCH_SLOTS = 256` u64 words
  (`compile.rs:996-1005`), ratified as out of scope (plan decision 6).
- **Not shipped:** `wasm/compile` is default-off (`crates/wasm/Cargo.toml:8-19`)
  and neither `runtime` nor `cli` enables it, so the CLI and the browser demo
  always run the interpreter; only `wasmtest --features compile` and
  `cargo test -p wasm --features compile` exercise it **(verified)**.
- **Doc drift:** the module doc says tables are 32-bit and "GC objects are not
  lowered yet" (`compile.rs:46`, `50-51`), but table64 and GC struct/array
  lowering both landed; `body_compile_reason` still names a `try_table` gate
  that `lowerable` no longer has (`compile.rs:418-419` vs `1085-1091`).

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
4. **The validator's `Unsupported` path is dead.** `valid::Error::Unsupported`
   (`valid.rs:28-31`) is never constructed, so any validator gap surfaces as a
   *failure* rather than *pending* — the honesty mechanism the crate documents
   is inert in phase 2. **Deferred:** out of scope for the 2026-09-14 decoder
   cleanup; wiring it moves validator verdicts corpus-wide and needs its own
   sweep.
5. **Supertype well-formedness is under-validated.** `build_spaces` bounds the
   supertype index but does not check composite-kind compatibility, supertype
   finality, or mutability/variance (`valid.rs:89-108`). `type-subtyping.wast`
   is excluded from the corpus, which likely hides this.
6. **Tokenless host functions cannot return results.** A *type-only* host
   function with a non-empty result type is `Unsupported("host function
   results")` from the top frame and in tail position (`exec.rs:5880-5888`,
   `6012-6013` **(verified)**). Real (token'd) host functions are unaffected;
   this is the `spectest`-stub path only.
7. **A dropped return value.** `write_memory`'s `bool` (a length-mismatch
   signal) is discarded (`runtime/.../wasm.rs:1457`) — a silent no-op if the
   invariant ever breaks.

### 6.3 Operational and harness honesty

- `wasmtest run` exits non-zero only on `fail`; `pending` never fails a sweep,
  so "0 pendings" is enforced by documentation, not by the gate
  (`wasmtest/src/main.rs:258-266`). `equiv` (the compile-vs-interpreter
  equivalence gate) and the coverage report require `--features compile` and
  are not part of the default sweep (`wasmtest/src/main.rs:303-333`).
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
| 5 | I | Shrink `Value` 32→16 bytes (box `V128` or a side v128 stack) (`values.rs:266`) | High | Med-High |
| 6 | C | Hoist/CSE bounds checks; cache descriptor loads, reloading after calls/grow (`compile.rs:1988-2031`, `1540`) | High | Medium |
| 7 | C | Native direct calls between compiled bodies (or inline the leaf fast path), raising the depth-64 cap (`compile.rs:3127-3131`, `exec.rs:650-683`) | High | High |
| 8 | J | Replace the full-memory memcpy bridge with zero-copy aliasing, syncing on grow/detach (`wasm.rs:1452-1502`, `3158/3174`) | High | High |
| 9 | I | Cut call-setup allocation: cache declared locals per body, reuse frames (`5938-5959`) | Med-High | Medium |
| 10 | C | Lower `v128.load` natively; reduce SIMD helper round-trips (`compile.rs:4878-4887`, `4817-4833`) | Med-High | High |
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
equivalence harness rather than new tests.

## 8. Recommended sequencing

1. **Correctness first, cheaply:** the body-trailing-bytes bug, the untrusted
   `with_capacity`, and the `Unsupported`/`Malformed` classification (6.2.1-3),
   plus wiring `valid::Error::Unsupported` so pending stays honest (6.2.4).
   These affect the trustworthiness of every conformance number.
   **Landed 2026-09-14:** 6.2.1-6.2.3, except 6.2.3's second half (see 6.2).
   6.2.4 is still open.
2. **Interpreter wins 1-4** — the highest impact/effort ratio in the engine,
   verify covered by `wasmtest equiv`.
3. **Make the compiler shippable or shelve it explicitly:** as it stands Cut 11
   is a large, default-off artifact with no native call linkage, so its
   real-world benefit is limited to call-free leaf bodies. Either gate it on
   (runtime/CLI feature) after adding direct native calls, or record the
   decision that it stays a research path.
4. **JS-API memory bridge** (item 8) — the only boundary cost that is a whole
   linear-memory copy per call once a `buffer` has been materialised.
5. Threads/atomics and `WebAssembly.Function` are the remaining whole-feature
   gaps; both are proposal-sized cuts, not cleanups.

## 9. How to re-run the gates

```sh
git submodule update --init waspec          # required for any wasm sweep
cargo run -p wasmtest -- run waspec/test/core/*.wast
cargo run -p wasmtest -- run waspec/test/core/simd   # and each proposal dir
cargo run -p wasmtest -- jsapi waspec/test/js-api
cargo run -p wasmtest --features wasmtest/compile -- run ...   # + equiv gate
```

`wasmtest equiv` (or `coverage`) is the compile-path gate; without
`--features compile` it is not exercised.

## 10. Suggested `.rules` additions

- "The wasm Cranelift backend (`wasm/compile`) is off by default and is not
  enabled by `runtime` or `cli`: the CLI and browser demo always run the
  interpreter. Enabling it makes every instantiate eagerly compile all bodies
  (`wasmtest`'s feature-unification warning exists for this reason)."
- "`wasmtest run` cannot fail on `pending`; a real gate must check the pending
  count, and `equiv`/`coverage` need `--features compile`."
