# WebAssembly (wasm) engine: implementation plan

This is the engineering spec for implementing a WebAssembly runtime *inside*
Slag — so JavaScript can compile and execute wasm modules through a
`WebAssembly` global, the way V8 embeds wasm next to its JS engine.

The **primary reference is the pinned `waspec` submodule** (the official
WebAssembly specification repository):

- `waspec/document/core/` — the core spec (structure, validation,
  execution, binary, text) as Sphinx source under `syntax/`, `valid/`,
  `exec/`, `binary/`, `text/`; `index.bs` is the same document in the
  new spectec tooling.
- `waspec/document/js-api/` — the `WebAssembly.*` JavaScript API spec.
- `waspec/test/core/` — the conformance corpus: **97 top-level `.wast`
  files** (the core suite) plus feature suites in subdirectories.
- `waspec/test/js-api/` — the JS-API test suite (WPT-style `.js`).
- `waspec/interpreter/` — the reference interpreter (OCaml), the
  authoritative semantic oracle for debugging mismatches.
- `waspec/spectec/` — the new formal-spec toolchain; the hand-supplied
  `WebAssembly.md` is its rendered snapshot of the core spec (convenient
  for offline reading; the `document/core` tree stays authoritative).

The **secondary reference is V8** (`v8/src/wasm`): the interpreter/liftoff
opcode semantics where the spec prose is ambiguous, the JS-API object
model, NaN handling policy, and the `wasm-module-builder.js` test helper
pattern.

Status: **Cut 0 done, Cut 1 largely done**. Decisions are ratified (wast2json
for the conformance harness, canonical quiet NaN, post-Cut-5 ordering,
`crates/wasm` + `wasm` feature wiring). Cut 1's decoders are implemented and
unit-tested (module sections + the full baseline instruction stream; 10
unit tests green, full workspace clippy clean, the real 7 MB demo module
decodes end to end). `wasmtest run` now converts suites with wabt's
`wast2json` (env `WAST2JSON`, then PATH) and executes commands, classifying
outcomes by what the current engine can judge (decode/`assert_malformed`
now; `assert_invalid` pending validation in Cut 2; invocations pending
execution in Cut 3) — `utf8-invalid-encoding.wast` is 176/176 pass. Files
wabt cannot parse (GC type bags and element items, module-linking/`instance`
syntax) are tracked in the runner's exclusion manifest rather than failing
the gate.

## 1. The conformance surface (measured)

`waspec/test/core` counts at pin time:

| Suite | `.wast` files | Gate for |
|---|---|---|
| Core (top level) | 97 | the baseline engine (Cuts 0-5) |
| `bulk-memory/` | 8 | bulk table/memory ops (Cut 5) |
| `exceptions/` | 4 | exception handling (Cut 6) |
| `simd/` | 59 | vector instructions (Cut 7) |
| `relaxed-simd/` | 7 | relaxed vectors (Cut 7) |
| `gc/` | 17 | GC types (Cut 9) |
| `memory64/` | 25 | 64-bit addressing (Cut 8) |
| `multi-memory/` | 41 | multiple memories (Cut 8) |

`waspec/test/js-api` holds **61 files** (the JS-API gate, Cut 10).

### Harness shape (the `.wast` problem)

The core tests are written in the S-expression **script format** (text
modules + `assert_*` commands), and the engine will **not parse text
format** (ch. 6 is out of scope for the runtime — the JS API only consumes
bytes). Conformance therefore needs a compile step, exactly like the
`unicode` build script and the `wat2wasm` step in the browser demo:

- **`wast`** (the maintained bytecodealliance/wasm-tools text parser, a
  *test/dev* dependency — never a runtime dependency) converts each `.wast`
  in-process into one `.wasm` per module plus a `.json` of commands
  (`module`, `assert_return`, `assert_trap`, `assert_invalid`,
  `assert_malformed`, `assert_unlinkable`, `assert_exhaustion`,
  `register`, `action`), matching the wast2json JSON shape.
- A Rust runner executes the JSON commands against the engine — decode →
  validate → instantiate → invoke — and reports failures per file/command.
  This mirrors the test262 sweep (`crates/test262`) and can live in a new
  `crates/wasmtest` crate (pinned corpus = `waspec/test/core`).
- Tests import a **`spectest`** host module (`print`, `print_i32`, ...);
  the runner installs it, following what V8/wasmtime do for the same suite.

The text round-trip checks in `waspec/test/core/run.py` (binary→text→
binary) are reference-interpreter concerns and do **not** apply here; we
only consume the converted binaries/JSON.

## 2. Scope and feature gating

The engine is built **interpreter-first** (mirroring the certified `Step`
VM): a correct, spec-faithful core interpreter now, a compiler pass later
(see "Future work"). Cuts are gated by the suites above; the top-level 97
files are the milestone that defines "the baseline engine works", and each
feature suite turns on behind its own cut.

The top-level suite already exercises the post-MVP features that merged
into the core spec and appear there directly — multi-value, sign
extension, saturating conversions, reference types (`ref.*`, table
instructions), tail calls (`return_call*`), extended-const initializers —
so "baseline" means MVP plus those merged semantics, *not* the raw 1.0
feature set.

## 3. Architecture

Following the crate-per-subsystem layout:

- **`crates/wasm`** (new, pure Rust, no third-party deps) — the core
  engine, laid out against the spec chapters:
  - `values.rs` — the value space: `i32/i64/f32/f64`, references, vectors
    later; bits-level f32/f64 with the IEEE helper ops the spec defines by
    hand (ch. 2.2 / 4.3).
  - `types.rs` + `module.rs` — the type system (ch. 2.3) and module
    structure (ch. 2.5): types, funcs, tables, memories, globals, tags,
    element/data segments, imports/exports.
  - `binary.rs` — the decoder (ch. 5): LEB128, sections, instruction
    stream, name/custom sections tolerated.
  - `valid.rs` — validation (ch. 3): the type-checking algorithm over
    decoded bodies; the compile pipeline is decode → validate → instantiate.
  - `exec/` — the runtime (ch. 4): the store (module/function/table/
    memory/global instances), an explicit-frame operand-stack machine
    (labels, frames, exception handlers), numeric semantics, traps.
- **`crates/runtime/src/wasm.rs`** + a `wasm` feature — the JS API
  (document/js-api): `WebAssembly.Module/Instance/Memory/Table/Global/Tag/
  Exception`, the error constructors, compile/validate/instantiate, and
  JS↔wasm bridging. Installed as `Context::install_wasm()`, following the
  `raylib`/`rlx`/`fs` host-module pattern; the CLI enables the feature so
  scripts get `WebAssembly` like V8 gives it to a page.
- **`crates/wasmtest`** (new) — the conformance runner + a JS-API harness
  (see Cut 10).

### Reused engine machinery

- Number ↔ JS conversion uses the existing embedding surface
  (`JsValue::number`, `bigint_from_latin1`/`bigint_sum` for i64, and the
  `Context` conversion helpers).
- `Memory` buffers are exposed through the existing ArrayBuffer helpers
  (`Context::array_buffer_from_bytes`, `as_bytes`, `set_bytes`) — the
  `memory.buffer` getter and the copy-in/out paths sit directly on them.
- JS function imports/exports become builtins via `Function::create_builtin`
  (the pattern already used by the DOM bridge in
  `crates/slag/examples/wasm_binding/dom.rs` and by `rlx`/`raylib`).
- Stack-guard / recursion-limit handling reuses the engine's existing
  approach so hostile wasm cannot smash the host stack (`stack.wast`,
  `skip-stack-guard-page.wast`).

## 4. Key semantic decisions (to ratify in code review)

- **NaN policy**: deterministic. The spec permits any NaN payload from an
  arithmetic operation, so the engine canonicalizes to the quiet NaN
  (V8-style) unless the operation's semantics require propagating a
  payload (`copysign`, `min`/`max` zero rules, pmin/pmax later). Tests only
  assert `nan:canonical`/`nan:arithmetic`, so this is conformance-neutral.
- **`fmin`/`fmax`/`nearest`** are *not* Rust's `f32::min/max` (sign-of-zero
  and NaN differ) — implement per spec 4.3.3 (signed zero rules,
  ties-to-even for `nearest`).
- **Traps** are an internal error kind that becomes `WebAssembly.RuntimeError`
  only at the JS boundary; inside the interpreter they unwind the operand
  stack to the nearest catch/try_table or the call edge.
- **No native recursion**: call frames live on an explicit heap-allocated
  stack, so call depth is governed by a configurable limit (not the C
  stack), matching ch. 4 semantics and `assert_exhaustion`.
- **Determinism**: numeric ops are total/defined per spec; no host
  nondeterminism leaks into the core (host calls are the only
  nondeterministic edge, as in JS).

## 5. The cuts

Each cut ends with its `.wast` gate green (via the runner) and a
`Definition of done`. Suggested `.rules`/test additions land in each cut's
review, per repo rules hygiene.

### Cut 0 — Scaffolding and harness plumbing

- Add `crates/wasm` (empty lib), `crates/wasmtest` (runner skeleton), wire
  both into `Cargo.toml` members; document the `wast2json` requirement and
  pin its version in a build note.
- `binary.rs` seeds: LEB128 read/write, magic/version check, and the module
  section *enumeration* (names and ids, unknown ids → malformed per ch. 5).
- Runner: parse a `wast2json` output, load modules, expose `spectest`
  imports, and run a hand-written `add` fixture end to end.
- DoD: `cargo test -p wasm` passes a decode+instantiate smoke; the runner
  prints per-file status for one converted file.

### Cut 1 — Decoder: types, modules, instructions

- Full module decoding (ch. 5.2-5.5): type/import/function/table/memory/
  global/export/start/element/data/data-count/code/custom sections;
  limits, indices, globals with const-exprs, element kinds.
- Instruction decoding for the baseline opcode space (numeric incl.
  sign-ext + saturating conversions, parametric, control incl.
  multi-value block types and tail calls, variable, memory, reference,
  table, bulk).
- Malformed-vs-valid is decided here for structure (a decode error that
  must be reported as `assert_malformed` vs a body that only `validation`
  rejects as `assert_invalid`).
- DoD: the decoder-oriented top-level files report correctly:
  `binary.wast`, `binary-leb128.wast`, `custom.wast`, `utf8-*.wast`,
  `names.wast`. (Text-format files such as `comments.wast`, `token.wast`,
  `id.wast`, `annotations.wast`, `inline-module.wast`,
  `obsolete-keywords.wast` still emit modules through `wast2json`, but their
  assertions are mostly text-parser concerns — low decoder value, kept in
  the gate as regression noise only once the suite runs.)

### Cut 2 — Validation (ch. 3)

- The type-checking algorithm: value/result/block typing, instruction
  typing for the baseline opcode space, const-expr rules for
  initializers, import/export type matching, start-function signature,
  element/data constraints.
- Compile pipeline becomes decode → validate → typed-module, so
  `assert_invalid` modules are rejected at instantiation time with the
  right *kind* of failure (malformed vs invalid), letting the runner
  dispatch correctly.
- DoD: `type.wast`, `unreached-invalid.wast`, `unreached-valid.wast`,
  `select.wast`, `type-equivalence.wast`, `type-rec.wast`,
  `type-canon.wast`, `global.wast` (invalid cases), `elem.wast`,
  `data.wast`, `imports.wast` (invalid cases) validate/dispatch correctly.

### Cut 3 — Execution core: values, numerics, locals, control flow

- The operand-stack machine: frames, labels, values (ch. 4.2, 4.6).
- Numeric semantics (ch. 4.3): every i32/i64/f32/f64 instruction with the
  exact wrap/truncate/extend/convert/rounding rules; integer div/mod and
  shift masking; float arithmetic with the hand-specified helpers
  (`nearest`, `min`/`max`, `sqrt`, `copysign`, NaN policy); traps.
- Parametric + control instructions: `drop/select`, `block/loop/if/else`,
  `br/br_if/br_table`, `return`, `nop`, `unreachable`; locals
  (get/set/tee); `assert_exhaustion` support.
- DoD (runnable parts): `i32.wast`, `i64.wast`, `f32.wast`,
  `f32_bitwise.wast`, `f32_cmp.wast`, `f64.wast`, `f64_bitwise.wast`,
  `f64_cmp.wast`, `conversions.wast`, `const.wast`, `int_exprs.wast`,
  `int_literals.wast`, `float_exprs.wast`, `float_literals.wast`,
  `float_misc.wast`, `block.wast`, `loop.wast`, `if.wast`, `br.wast`,
  `br_if.wast`, `br_table.wast`, `switch.wast`, `return.wast`,
  `labels.wast`, `nop.wast`, `unreachable.wast`, `stack.wast`,
  `local_get.wast`, `local_set.wast`, `local_tee.wast`, `local_init.wast`,
  `traps.wast`, `skip-stack-guard-page.wast`.

### Cut 4 — Functions, calls, globals, memory, instantiation

- Store and instance allocation (ch. 4.2, 4.7): function/global/memory/
  table instances; module instantiation with imports; start function;
  exports.
- Calls: `call`, `return_call` (tail), params/results, and JS/host calls at
  the boundary.
- Linear memory (ch. 4.6.8): load/store with alignment rules, endianness,
  `memory.size`/`memory.grow` (64KiB pages, limits), traps; data segments.
- DoD: `func.wast`, `call.wast`, `forward.wast`, `fac.wast`, `start.wast`,
  `global.wast`, `memory.wast`, `memory_trap.wast`, `memory_size.wast`,
  `memory_grow.wast`, `memory_redundancy.wast`, `load.wast`, `store.wast`,
  `address.wast`, `align.wast`, `endianness.wast`, `data.wast`,
  `float_memory.wast`, `left-to-right.wast`, `exports.wast`,
  `imports.wast`, `linking.wast`, `instance.wast`, `return_call.wast`,
  `return_call_indirect.wast` (function half once tables land in Cut 5).

### Cut 5 — Tables, reference types, call_indirect, bulk memory

- Tables + element segments (active/passive/declarative); `call_indirect`,
  `return_call_indirect`; `table.get/set/size/grow/fill/copy/init`,
  `elem.drop`.
- Reference types: `ref.null/is_null/func`, typed `select`,
  `ref.eq`, `br_on_null`, `br_on_non_null`, `ref.as_non_null`; typed
  function references (`call_ref`, `return_call_ref`) are exercised by
  top-level files, so func-ref typing is in the baseline (not deferred to
  Cut 9).
- Bulk memory: `memory.copy/fill/init`, `data.drop`.
- **Milestone: the top-level 97-file core suite is green** — this is the
  "baseline engine" definition of done.
- DoD: `call_indirect.wast`, `func_ptrs.wast`, `table.wast`,
  `table_get.wast`, `table_set.wast`, `table_size.wast`, `table_grow.wast`,
  `elem.wast`, `ref.wast`, `ref_null.wast`, `ref_is_null.wast`,
  `ref_func.wast`, `ref_as_non_null.wast`, `br_on_null.wast`,
  `br_on_non_null.wast`, `call_ref.wast`, `return_call_ref.wast`,
  plus `bulk-memory/` (8).

### Cut 6 — Exceptions

- `tag` types/instances, `throw`, `try_table` (and legacy `try/catch` if
  the corpus still exercises it), exception references, unwind rules.
- DoD: `exceptions/` (4) and the `Tag`/`Exception` half of the JS API
  (can land with Cut 10's API work or standalone via the runner).

### Cut 7 — SIMD and relaxed SIMD

- v128 value type; the full vector instruction set (constants, loads/
  stores incl. lane and zero-extend forms, lane ops, shuffles, splats,
  comparisons, shifts, bitmask, ternops) with the ch. 4.3.5 semantics.
- Relaxed ops under the relaxed-simd deterministic subset (spec marks
  results nondeterministic; pick one legal interpretation and document it).
- DoD: `simd/` (59) then `relaxed-simd/` (7).

### Cut 8 — Memory64 and multi-memory

- Index type `i32|i64` on memories; 64-bit address arithmetic on loads/
  stores; `memory.size/grow` with i64; limits encoding.
- `memidx` on memory instructions; multiple memories per module and
  instantiation rules.
- These are engine-wide address-model changes; expect them to re-touch
  `binary.rs`, `valid.rs`, and the memory instructions together.
- DoD: `memory64/` (25) and `multi-memory/` (41).

### Cut 9 — GC (structs, arrays, ref casts)

- Heap types (`any/eq/struct/array/i31`), recursive types and type
  canonicalization, `struct.*`/`array.*` instructions, `ref.cast`/
  `ref.test`/`br_on_cast*`, `ref.eq` semantics over GC refs.
- Expected to be the deepest change to the *type system* (validation and
  subtyping), so it is sequenced after the interpreter is stable.
- DoD: `gc/` (17).

Status (Cut 9 waves 1-3 landed): type model + decoder (wave 1), GC
instruction typing (wave 2), and the GC object runtime — struct/array/i31
extern values, `struct.*`/`array.*`, `ref.cast/test`/`br_on_cast*` with
canonical (structural) type matching — land the corpus. `gc/` runs 653
pass / 0 fail with 16 of 17 files green under the default exclusions
(`struct.wast` keeps one harness `assert_malformed` quote-text command
pending; `type-subtyping.wast` stays excluded because its multi-supertype
fixtures exceed both the `wast` grammar and the decoder's single-supertype
model). `type-canon.wast` and the bulk-memory/`memory64` `table_init`
files were un-excluded with the wave.

### Cut 10 — The JavaScript API (document/js-api)

- `WebAssembly` namespace object and constructors:
  - `Module` (compile/validate/customSections/exports/imports),
    `Instance` (exports, linking), `Memory` (buffer/grow), `Table`
    (get/set/grow/type), `Global` (value/valueOf), `Tag`, `Exception` once
    Cut 6 lands.
  - Errors: `CompileError`, `LinkError`, `RuntimeError` with the proper
    prototype/`toStringTag` relationships.
- BufferSource handling for compile/instantiate bytes (the existing
  `ArrayBuffer`/`TypedArray` machinery); JS↔wasm argument/result
  conversion incl. BigInt for i64 and the wasm value ↔ JS number rules.
- JS imports: function imports become host-callable closures; memory/
  table/global imports resolve from other instances or JS objects;
  `spectest`-style linking errors map to `LinkError`.
- DoD (revised 2026-09-06): the `test/js-api` suite (61 files) runs to a
  per-file verdict through the WPT-lite harness (no fixture crashes or
  hangs the sweep); driving every fixture green is follow-up wave work,
  not the cut's gate.

Status (wave decomposition): the JS-API layer lives in a new runtime
builtin (`crates/runtime/src/builtins/wasm.rs`) backed by the `wasm`
crate, installed during SetDefaultGlobalBindings and dispatched by
intrinsic identity like every other agent-dependent builtin.

- Wave 1 (landed): the `WebAssembly` namespace and all constructor/
  prototype interface shapes (`interface.any.js` shape subset, incl.
  `Tag`/`Exception`), the
  `CompileError`/`LinkError`/`RuntimeError` classes (reusing the native
  error machinery), and a working `WebAssembly.validate`. The namespace
  operations and prototype methods are real agent-dispatched functions;
  the ones behind later waves reject with a clear error until their wave.
  Verified by `builtins::wasm::tests` in the runtime crate.
- Wave 2 (landed): `WebAssembly.Module` compiles bytes (CompileError on
  invalid modules) and its `exports`/`imports`/`customSections` accessors
  report descriptor records and fresh ArrayBuffers per matching custom
  section (the Agent now keeps a [[Module]] record per Module object).
- Wave 3a (landed): a per-agent engine store, `WebAssembly.Instance`
  (module object or bytes) with a prebuilt `exports` object, and
  JS-callable exported functions (memoized dispatch by function id;
  numeric argument/result conversion). Function imports, non-function
  export wrappers, and the memory buffer bridge remain in wave 3b.
- Wave 3b slice 1 (landed): `WebAssembly.Memory` over engine store
  cells — constructor with `[EnforceRange]` page descriptors, a
  `buffer` getter whose ArrayBuffer keeps object identity until a grow
  detaches it, and `grow` — plus the shared memory-buffer bridge: each
  memory's ArrayBuffer is a cache of its engine cell, flushed in before
  and refreshed after every wasm run (instantiation start functions
  included), so JS typed-array writes reach wasm and wasm writes reach
  JS at each call boundary. Instance exports now surface memory exports
  as `Memory` wrappers, and function wrappers are memoized per engine
  function; imports-object linking resolves exported wasm functions and
  `WebAssembly.Memory` wrappers (missing/wrong values are LinkError).
- Wave 3b slice 2 (landed): `WebAssembly.Table` (funcref; `length`,
  `get`/`set`/`grow`, constructor + element/init handling) and
  `WebAssembly.Global` (`value` getter/setter on mutable cells,
  `valueOf`, constructor) over engine store cells; Instance exports
  surface table/global exports as their wrapper objects; imports
  resolve `Memory`/`Table`/`Global` wrapper objects and tag imports
  fail cleanly (wave 4).
- Wave 3b slice 3 (landed): the engine host-function bridge — external
  engine host functions with an embedder token, and a resumable run
  driver (`Store::start`/`Store::resume`/`Store::abandon`) that parks
  the interpreter at each external-host call so the runtime can run the
  JS closure with the store unborrowed (fully reentrant: the closure
  may call another wasm export). Raw JS closures are callable as
  function imports (typed by the import declaration, converting numeric
  args/results), traps surface as `WebAssembly.RuntimeError`, a
  throwing closure abandons the run, and the memory-buffer bridge also
  runs around each host call so closures see current memory. Verified
  by `builtins::wasm::tests` (calls, reentrancy, throwing, memory
  reads) with no interpreter regression across the core call/exception
  suites. Raw JS closures as *funcref table elements* still need typed
  `WebAssembly.Function` wrappers (a later cut), and BigInt i64 and
  memory64 value conversions remain out of wave 3b.
- Wave 4 slice 1 (landed): promise-returning `WebAssembly.compile` and
  `WebAssembly.instantiate`. Both never throw synchronously: an
  `async_operation` helper settles a fresh promise from the result of a
  synchronous body (rejecting with the error's throwable — CompileError,
  LinkError, RuntimeError, or TypeError). `compile(bytes)` resolves with
  a `Module` (shared compile helper with the `Module` constructor);
  `instantiate` supports both overloads — a BufferSource compiles then
  instantiates and resolves with `{ module, instance }`, and a
  `WebAssembly.Module` instantiates directly, resolving with the
  `Instance`. The shared instantiation body was split out of the
  `Instance` constructor (`instantiate_module`) so both entry points
  link imports identically. Verified by the runtime wasm tests
  (resolve/reject paths over async/await with the job queue).
- Wave 4 slice 2 (landed): `WebAssembly.Tag` and `WebAssembly.Exception`
  objects plus wasm<->JS exception interop. Tags are standalone engine
  tag cells (`Store::tag`) behind memoized wrapper objects (identity per
  cell across constructor/import/exports), constructible from a
  `parameters` descriptor (numeric value types this wave). Exceptions
  store their tag cell + engine payload; `is`/`getArg` brand- and
  range-check (payload indices and tag matching). Tag imports resolve
  `WebAssembly.Tag` wrappers and tag exports surface as wrappers on
  `Instance.exports`. At the JS boundary: a wasm `throw` whose tag has a
  JS wrapper surfaces as a catchable `WebAssembly.Exception` (internal
  tags become RuntimeError); a JS-thrown `Exception` from an imported
  function is delivered into wasm as an in-flight exception
  (`Store::resume_exception`), so a matching `try_table` catches it with
  the payload as branch values, while an uncaught one rethrows the
  original JS value with identity preserved. The streaming entry points
  (`compileStreaming`/`instantiateStreaming`) are Web API — they need a
  fetch `Response` — and are deliberately out of scope (see Future
  work); the namespace does not install them.
- Wave 5 slice 1 (landed): the JS-API harness — `wasmtest jsapi <file|dir>`
  drives the `waspec/test/js-api` testharness-style fixtures through the
  Slag embed. A compact embedded shim implements the testharness surface
  the corpus uses (`test`/`promise_test`/`async_test`/`setup`/`done`, the
  `assert_*` family incl. `throws_js`/`throws_exactly`/`array_equals`/
  `class_string`/`own_property`/`inherits`, `promise_rejects_js`, and
  `format_value`), recording into `globalThis.__WPT` which the runner reads
  back as JSON after the job queues drain (WPT's own 5000-line
  `testharness.js` overflows the engine's parser, so a shim is used). Each
  fixture runs in a fresh agent with its `// META: script=` sibling helpers
  (root `assertions.js`, `wasm-module-builder.js`, `bad-imports.js`, the
  per-dir `assertions.js`, `instanceTestFactory.js`, js-string polyfill)
  preloaded in order; files registering no tests (the helpers themselves)
  report as `skip`, and `WASM_JSAPI_VERBOSE=1` prints the failing-test
  messages for triage. Green on the self-contained fixtures (error
  interfaces, `toString` shapes, etc.); fixtures that drive
  `wasm-module-builder.js` currently crash the engine with a stack
  overflow when the built modules exercise it (an interpreter/JIT bug the
  next slice fixes). Note: tc39/test262 no longer vendors WebAssembly
  fixtures (they moved to the js-api suite), so the plan's "vendored
  test262 built-ins/WebAssembly" arm is obsolete — the js-api suite is the
  whole JS-API gate.
- Wave 5 slice 2 (measured): the full 61-file corpus, each fixture in its
  own process so a crash cannot abort the sweep (the crash-class root
  cause is now precise and branch-independent — see the decision note in
  section 8). Of the 61 files: 7 helpers report `skip` (the builder,
  `assertions.js`, `bad-imports.js`, `instanceTestFactory.js`, the per-dir
  `assertions.js`), `js-string/polyfill.js` is a harness-error helper
  (needs its META builder context), **11 files are fully green**
  (`interface.any.js` 72/72, the `toString`/`valueOf`/`buffer`/`length`/
  `tag/constructor` shapes, `error-interfaces-no-symbol-tostringtag.js`),
  **10 files fail with real engine gaps** (below), and **32 files crash**
  with the debug-build stack overflow (every one loads
  `wasm-module-builder.js` and nests past the ~6-frame main-thread
  budget). The failing-file gaps, by class:
  - **BigInt/memory64 conversions** (the deferred wave-3b/4 item):
    `Global` i64 value/descriptor conversions, `Memory`/`Table` i64
    (memory64) `initial`/`maximum` and grow arguments, `Exception` i64
    `getArg` parameters — the constructor/`grow` bodies reject with the
    staged "not supported" TypeError, and the "exceeds maximum"
    order-of-checks tests see that TypeError instead of RangeError
    (`global/constructor.any.js`, `global/value-get-set.any.js`,
    `memory/constructor-memory64.any.js`, `memory/grow-memory64.any.js`,
    `table/constructor-memory64.any.js`, `table/grow-memory64.any.js`,
    `exception/constructor.tentative.any.js` i64 tag).
  - **Immutable/descriptor order-of-evaluation bugs**: setting an
    immutable `Global.value` must throw before any ToNumber of the
    argument — instead the engine reads a null somewhere and throws
    "Cannot read properties of null" (all `Immutable * with ToNumber
    side-effects` cases in `global/value-get-set.any.js`); the `Global`
    descriptor reads `mutable` before `value` and the `Memory` descriptor
    reads the wrong property first (`Order of evaluation` cases in
    `global/constructor.any.js` and `memory/constructor.any.js`).
  - **`Object.prototype.toString` after mutating the namespace's
    `@@toStringTag`** reads null (`constructor/toStringTag.any.js`).
  - **`Exception` interface gaps**: the constructor's `length` is 1
    (spec 2), and `Exception.prototype.is` with missing/plain-object
    arguments does not throw (`exception/constructor.tentative.any.js`,
    `exception/is.tentative.any.js`).
  - `js-string/` (the WebAssembly.JSString JS proposal, out of scope) and
    `gc/` (its JS-API surface needs `WebAssembly.Function` typed-function
    wrappers, a later cut) crash today and will fail cleanly once the
    stack fix lands — written taxonomy entries for both.
  The next slice fixes the small engine gaps above, then the BigInt
  conversion wave, then re-measures after the upstream stack fix lands.
- Wave 5 slice 3 (landed): the small engine/harness gaps from slice 2's
  measurement. Runtime fixes in `crates/runtime/src/builtins/wasm.rs`:
  `WebAssembly.Exception`'s constructor `length` is 2 (spec), not 1;
  `Exception.prototype.is` type-checks its argument (a missing or non-Tag
  value is a TypeError; a different Tag is false); the `Global.value`
  setter rejects an immutable cell *before* converting the argument (the
  spec's order — the argument's `valueOf` must not run); and the `Global`
  descriptor is read in spec order (`mutable` first, then `value`, then
  the initial value). Harness fix in `crates/wasmtest/src/jsapi.rs`: the
  shim's `test`/`promise_test` now pass a WPT-style test object
  (`unreached_func`, `step_func`, `add_cleanup`, `done`), so fixtures
  using `t.unreached_func`/`t.add_cleanup` no longer fail spuriously
  reading a property of null. Re-measure on the shallow fixtures:
  `exception/constructor.tentative.any.js` 5/6 (only the deferred i64 tag
  param remains), `exception/is.tentative.any.js` 3/3 (green),
  `global/constructor.any.js` 50/62, `constructor/toStringTag.any.js`
  4/4 (green). The deep fixtures are still unmensurable until the engine's
  upstream stack fix lands (see section 8 decision 5), and the remaining
  fails are the BigInt/memory64 wave (below) plus the `Memory` descriptor
  `address` member.
- Wave 5 slice 4 (landed): the harness runs the whole corpus. Each fixture
  runs on a dedicated 64 MiB-stack thread (the `run_deep` budget in
  `crates/runtime/src/eval.rs`) so the debug interpreter's ~160 KB/frame
  cost no longer kills the sweep at the first deep fixture — the engine
  stack workaround lives in the harness (`crates/wasmtest/src/jsapi.rs`),
  and a fixture that still exhausts the budget surfaces as that file's
  error instead of aborting the run. The js-api runner now honors the
  shared exclusion manifest (`wasm-exclusions.txt`), and
  `limits.any.js` (the WPT `timeout=long` embedder-limit file, which
  builds and validates million-element modules) is excluded with a written
  reason. `wasmtest jsapi waspec/test/js-api` completes in ~11 s with all
  61 files verdicted: 24 pass, 29 fail, 7 helper skips + 1 exclusion, and
  501 tests pass / 273 fail. The failing files are the actionable follow-up
  list: the BigInt/memory64 conversion wave (the `Memory`/`Table`
  memory64 `initial`/`maximum`/grow + the `address` descriptor member,
  `Global` i64, `Exception` i64 tag params), `externref`/reference JS
  globals, the `js-string`/`gc` suites (out-of-scope JSString proposal /
  `WebAssembly.Function` typed-function wrappers), and per-file gaps now
  visible per-fixture.
- Wave 5 slice 5 (landed): the i64 value-conversion half of the BigInt
  wave. JS↔wasm `i64` now converts through ToBigInt (wrapping modulo
  2^64; `crux::BigInt::to_i64_wrapping`) for value results, and BigInt
  results convert back — unlocking `WebAssembly.Global` i64 (default 0n,
  value get/set, ToBigInt constructor conversions incl. objects),
  `WebAssembly.Tag`/`Exception` i64 parameters and payloads, and i64
  function-import/export arguments. Corpus: 793 tests pass / 297 fail
  (was 501/273), with `global/constructor.any.js` 60/62,
  `global/value-get-set.any.js` 68/69, `exception/constructor.tentative`
  and `exception/getArg` fully green, and the previously-eval-failing
  `constructor/instantiate-bad-imports.any.js` now running its 212 tests
  (176/36). Remaining BigInt-wave work: memory64/table64 `address`
  descriptors, BigInt page limits and grow, the grow EnforceRange error
  kind, plus the separate follow-up classes surfaced by the run:
  null-prototype/immutable `Instance.exports`, primitive and `anyfunc`
  global/table import linking, `customSections` array shape, externref
  globals, multi-value results, and shared memory.
- Wave 5 slice 6 (landed): the memory64/table64 half of the BigInt wave.
  `Memory`/`Table` descriptors now read the `address` member ("i32"/"i64",
  default i32) first and convert `initial`/`maximum`/grow/indices in the
  address's index domain — EnforceRange u32 Numbers for i32, WebIDL-`bigint`
  u64 for i64 (`crux::BigInt::to_u64`; an unparseable string is a TypeError,
  matching WebIDL rather than ToBigInt's SyntaxError). Memory64/table64
  cells carry the engine's `memory64`/`table64` flags (new `Store::memory_type`/
  `table_type` accessors); `length` and `grow` results are BigInts for
  i64-address objects, and the grow argument errors are TypeErrors
  (EnforceRange) rather than the old RangeError. New green files:
  `memory/constructor.any.js` (29/29), `memory/constructor-memory64.any.js`
  (10/10), `memory/grow-memory64.any.js` (8/8), `table/constructor-memory64.any.js`
  (12/12), `table/grow.any.js` (18/18); `memory/grow.any.js` 18/19 (the one
  fail is the shared-memory detach test, threads out of scope),
  `table/constructor.any.js` 40/41 (externref table), `table/get-set.any.js`
  39/41 (externref/closure elements). Corpus: 849 tests pass / 241 fail
  (was 793/297), 30 fixture-files green of 53. The BigInt/memory64 wave is
  done; what remains is the follow-up classes: externref/reference JS
  values (globals, tables, `WebAssembly.Function` closures), instance
  linking/exports shape, multi-value results, `customSections`, shared
  memory, and the out-of-scope `js-string`/`gc` suites.
- Wave 5 slice 7 (landed): instance shape, global-import linking, exported
  function objects, and a harness fix. `Instance.exports` is now a
  null-prototype, non-extensible object with non-writable,
  non-configurable, enumerable data properties (the JS-API module-exports
  shape). Global imports accept a `WebAssembly.Global` wrapper (the engine
  type-checks it) or — for an immutable import only — a value of the exact
  JS kind for the type (Number for i32/f32/f64, BigInt for i64), any other
  value being a LinkError (not a TypeError). Exported-function wrappers now
  chain to `%Function.prototype%` (they only use WebAssembly.Function's
  prototype when that interface exists) and carry the function's index as
  their `name` (the wrapper is shared across export names). The shim's
  `assert_array_equals` accepts array-likes (typed arrays), fixing
  `module/customSections.any.js`. Newly green: `instance/constructor.any.js`
  (29/29), `module/customSections.any.js` (9/9); `instance/exports`,
  `module/exports`, `module/imports` stay green. `constructor/instantiate.any.js`
  (30/63 — the remaining fails are the instantiate result-object overload
  checks and option handling). Corpus: 898 tests pass / 192 fail (was
  849/241), 32 fixture-files green of 53.
- Wave 5 slice 8 (landed): the `instantiate` overloads and a GC liveness
  fix. `Agent::trace_roots` now traces the wasm JS-API value tables
  (`wasm_instance_exports`, `wasm_func_objects`, `wasm_memory_buffers`,
  `wasm_host_functions`, `wasm_tag_objects`, `wasm_js_exceptions`): an
  untraced `Value` there was freed by a collection and its object id
  reused, so a later `Instance.exports` (or wrapper) lookup returned a
  foreign object (a Promise/function/instance) once a fixture registered
  enough instances to trigger a GC mid-file. `WebAssembly.instantiate`'s
  BufferSource overload now defers its compile + imports-reading to a
  microtask (a `deferred_operation` generic job; the byte argument is
  copied synchronously) per the JS-API's synchronous-options rules, while
  the Module overload keeps reading imports synchronously.
  `constructor/instantiate.any.js` is fully green (63/63). Corpus: 931
  tests pass / 159 fail (was 898/192), 33 fixture-files green of 53.
  Remaining: externref/reference JS values, multi-value results, shared
  memory, the `table/grow-memory64` nulls-coupling fixtures, the residual
  bad-imports/exception gaps, and the out-of-scope `js-string`/`gc`
  suites.
- Wave 5 slice 9 (landed): externref JS values. The JS-API `externref` is
  an arbitrary JS value (null and undefined included, round-tripping
  distinctly); the agent keeps each one alive behind an opaque
  `ExternInner::Host` token (`wasm_extern_values`, traced in
  `Agent::trace_roots`) and the boundary converts through it.
  `WebAssembly.Global` accepts `externref` (default `undefined`; value
  get/set round-trips any JS value) and `WebAssembly.Table` accepts an
  `externref` element (initial/fill, get/set, grow). Newly green:
  `global/constructor.any.js` (62/62), `global/value-get-set.any.js`
  (69/69), `table/constructor.any.js` (41/41); `table/get-set.any.js`
  40/41 (the last fail is raw-JS-closure funcref elements, which need
  typed `WebAssembly.Function` wrappers). Corpus: 936 tests pass / 154
  fail (was 931/159), 36 fixture-files green of 53.
- Wave 5 slice 10 (landed): JS-API import-argument validation. Per the
  spec's argument checks (as the `bad-imports.js` corpus shared by
  `instance/constructor-bad-imports` and `constructor/instantiate-bad-
  imports` exercises): a missing imports argument on a module that
  declares imports, a non-object imports argument, and a missing or
  non-object module namespace are TypeErrors; only a missing import name
  inside a present object namespace (or a present but wrong-typed value)
  is a LinkError. Previously all of those were LinkErrors. The two
  runtime engine tests that asserted the old behavior for the
  missing-imports cases now expect TypeError (the wrong-kind cases stay
  LinkError). Newly green: `instance/constructor-bad-imports.any.js`
  (106/106) and `constructor/instantiate-bad-imports.any.js` (212/212).
  Corpus: 990 tests pass / 100 fail (was 936/154), 38 fixture-files
  green of 53.
- Wave 5 slice 11 (landed): wasm-exception reification and wrapper-identity
  caching. An escaping tagged wasm exception now always reifies as a
  `WebAssembly.Exception` — JS needs no `WebAssembly.Tag` wrapper for the
  tag, so an internal tag that was neither imported nor exported surfaces
  as one too (previously RuntimeError). `exception/basic.tentative.any.js`
  is fully green (6/6). The JS-API wrapper memos now key on the underlying
  engine identity rather than the surface: exported-function wrappers key
  by the canonical engine function (`Store::func_key` resolves imported
  aliases to their defining instance), and memory/table/global wrappers get
  a per-cell memo registered by the constructors too — so a module
  re-exporting a function/global/memory/table it imported surfaces the very
  same JS objects the importer passed in. `instance/constructor-caching`
  is green (1/1). Corpus: 994 tests pass / 96 fail (was 990/100), 40
  fixture-files green of 53. Runtime suite: 725 tests pass.
- Wave 5 slice 12 (landed): multi-value across the JS boundary, plus a
  harness fixture patch and out-of-scope exclusions. A wasm export with
  several results now returns a fresh Array of the converted results (in
  order) instead of erroring, and an imported JS function whose declared
  type has several results runs the return value through GetIterator /
  whole-sequence IteratorStep and converts each element by the result
  types once iteration completes (the `constructor/multi-value.any.js`
  observer ordering holds exactly). `constructor/multi-value.any.js` is
  green (3/3). The jsapi runner gained a fixture-source patch table for
  defects in the pinned spec snapshot (`grow-memory64.any.js` uses
  `nulls(n)` but the commit that split it — `2929f4497` — never moved the
  helper out of `grow.any.js`); that file is green (6/6). The `js-string`
  and `gc` JS-API suites are now listed in `wasm-exclusions.txt` as out
  of scope (no later cut owns them). Corpus: 997 tests pass / 4 fail (was
  994/96), 42 fixture-files green with the 3 remaining files being
  `exception/jsTag` (needs `WebAssembly.JSTag`), `memory/grow`
  (shared-memory detach), and `table/get-set` (raw JS closures in funcref
  tables need typed `WebAssembly.Function`). Runtime suite: 726 tests
  pass.
- Wave 5 slice 13 (landed): the last two in-scope gaps plus the sweep
  taxonomy. `WebAssembly.JSTag` (JS-API 4.13): a Tag-shaped object whose
  externref-payload cell materializes on first use; an arbitrary JS value
  thrown by an imported function is now wrapped in the JS tag (so a wasm
  `try_table` catch for it — or `catch_all` — intercepts the value, whose
  externref payload reaches wasm), and a JS-tag exception that escapes —
  whether wasm-originated (a `throw` of the tag) or a rethrow of an
  injected one — surfaces as the original JS value, never a
  `WebAssembly.Exception`. Constructing an exception with the JSTag stays
  a TypeError, and the Tag constructor accepts an `externref` parameter.
  `exception/jsTag.tentative.any.js` is green (3/3). `table/get-set`
  (41/41): a funcref `Table.prototype.set` distinguishes an omitted value
  (clears to null) from an explicit `undefined` (TypeError, since it is
  neither callable nor null). `memory/grow.any.js` (18/19 — its one
  failing test is the shared-memory grow-detach rule) joins
  `wasm-exclusions.txt` as a not-planned proposal, like `js-string`/`gc`.
  Corpus: the sweep is fully green — 44 fixture-files pass / 0 fail, 982
  tests pass / 0 fail (the excluded proposals and `limits.any.js` run on
  demand). Runtime suite: 728 tests pass.
- Wave 5 slice 14 (landed): shared-memory JS-API semantics. A
  `WebAssembly.Memory` descriptor with `shared: true` (a maximum is
  required) now allocates a shared engine cell whose `buffer` is a
  SharedArrayBuffer over a per-cell byte block; `Memory.prototype.grow` on
  a shared memory keeps the previous buffer attached and pointing `buffer`
  at a fresh SAB over the same resized block, so old and new buffers keep
  aliasing the memory (the JS-API shared grow-detach rule — only an
  unshared grow detaches). The wasm-side refresh path (a shared memory
  that grew inside a run) does the same instead of detaching. This also
  fixed a general spec gap: SharedArrayBuffer instances are now
  nonextensible (`Object.isFrozen` true, spec 25.3.3).
  `memory/grow.any.js` is fully green (19/19) and left the exclusions.
  Corpus: 1001 tests pass / 0 fail, 45 fixture-files green of the runnable
  set (`limits.any.js` stays excluded: embedder-limit conformance whose
  `maxMemories = 1` subtest conflicts with the landed multi-memory
  support). Runtime suite: 729 tests pass.
- **Cut 10 exit — standalone WebAssembly smoke (landed 2026-09-06):** the
  `wasm_smoke` example's default self-test compiles, instantiates, and
  calls a wasm module from JS (`add(20, 22)` → 42) and reads a
  `WebAssembly.Memory`/`Global`; `cargo run -p slag --example wasm_smoke`
  asserts the output and exits non-zero on any mismatch. The browser demo
  pages were intentionally left untouched — wasm correctness is gated by
  the `wasmtest` CLI corpora, not by the dogfood UI.

### Cut 11 — Compilation: wasm-to-native via Cranelift (planned, not started)

Not a conformance cut: the interpreter is green against the full corpus and
stays the correctness oracle. Cut 11 is a performance path — a wasm-to-
native compiler over the existing Cranelift dependency, reusing the Slag
JIT's `Step`-lowering lessons — that must not change observable behavior.

Scope:
- A compile stage after `validate` succeeds: lower each function body to a
  Cranelift function whose ABI mirrors the interpreter's value model,
  passing the store, instance, and memory/table cells by handle.
- The call graph: direct wasm-to-wasm calls stay native; calls into
  imported host/JS functions and host calls into compiled wasm cross the
  same boundary the interpreter uses (the resumable `HostRequest`/
  `RunProgress` split), so compiled frames must be unwound or recorded at
  host-call boundaries.
- Fallbacks: any body the compiler cannot lower keeps the interpreter's
  `Step` loop, giving a per-function compiled/interpreted split with a
  shared value protocol.
- Stack: preserve the depth limit and `assert_exhaustion` behavior with a
  Cranelift-native guard (the interpreter's `DEFAULT_DEPTH_LIMIT`
  contract).

Key decisions to ratify in review (mirroring the Slag JIT's conventions):
- The helper ABI for memory/table access, host calls, GC allocation, and
  traps, plus how `throw`/`try_table` exceptions lower (the interpreter's
  in-flight `ExceptionInst` pool vs native unwind).
- A compile threshold (the Slag JIT's `Cut 69` analogue): compile simple /
  hot bodies only, dispatch everything else to the interpreter.
- Whether GC `RefValue`s stay pool ids (objects are already store-pool ids,
  so cross-compiled/interpreter references should need no new
  representation).

Verification: an equivalence harness runs the corpus's `action`/
`assert_return` commands through both paths with compilation forced and
compares outcomes bit-for-bit (results, trap kinds, NaN patterns,
exceptions). Compiled-only runs must reproduce the Cut 10 totals
(baseline core 20,662 / 0 / 0 and the feature dirs).

#### Cut 11 wave breakdown (2026-09-06)

The interpreter (`Engine::step`, flat stack machine over a decoded
`Vec<Instr>`, store-pool instances/frames, resumable host boundary) stays
the oracle; each wave adds a lowerable subset and lands with the
equivalence gate green over it plus clippy clean. `crates/jit` is bound
to the JS `Step` VM and is not reused; the wasm compiler lives in
`crates/wasm` behind an optional `compile` feature pulling
cranelift-codegen/frontend 0.134.3 (the pinned workspace version).

- **Wave 0 — substrate + equivalence gate.** Compile pass after
  `validate`: per-module, per-defined-function compiled entries held by
  the `Instance`/`Store`. `wasmtest` gains a compile-forced mode that runs
  corpus actions through compiled bodies (supported modules) and
  interpreter bodies (unsupported, so totals stay comparable) and asserts
  bit-for-bit equality on every action where the path was compiled.
  Gate: equality holds on the supported subset and the interpreter's
  corpus totals are reproduced.
- **Wave 1 — numeric leaf core.** Lower `const`/`Num`/`local.*`/`drop`/
  `select`/`block`/`loop`/`if`/`else`/`br`/`br_if`/`br_table`/`return`/
  `nop`/`unreachable`; no memory, globals, calls, refs. Leaf functions
  only (callee-free), so no host boundary yet. Value model: params and
  locals are Cranelift SSA values (native i32/i64/f32/f64), not
  interpreter `Value`s. Canonical-quiet-NaN arithmetic policy must be
  reproduced exactly (Cranelift does not canonicalize): NaN canonicalized
  after ops via an explicit mask/or or a helper. Trap-raising ops
  (`unreachable`, integer div-by-zero/overflow) lower to trap-code
  returns, not Cranelift `trap`.
- **Wave 2 — memory, globals, tables.** Loads/stores with OOB traps,
  `memory.size`/`grow`, memory64/multi-memory, `global.*`, and the
  `table.*`/`call_indirect`/`ref.func` family. Compiled code gets a
  context pointer (store + instance + memory/table cells) and calls
  helpers for the operations that cannot inline safely.
- **Wave 3 — calls and the host boundary.** Direct wasm-to-wasm calls
  stay native when both sides compile (trampolines marshal args/results
  where a compiled/interpreter boundary is crossed). Functions whose
  callee graph can reach a resumable external host import are not
  compiled (fall back to the interpreter's parked-run protocol), keeping
  one host-call mechanism.
- **Wave 4 — traps, exceptions, refs, GC, SIMD.** `throw`/`try_table`
  unwinding across compiled frames (compiled frames return a trap/exn
  status to their caller; only frames enclosing a `catch` stay
  interpreter-side initially), then pool-id ref values, GC struct/array
  and i31, and v128/relaxed ops (the interpreter's `simd_exec` semantics
  are the oracle).

Open design points to ratify as waves start (mirroring the JIT's
conventions): the compiled entry ABI (values by Cranelift signature;
results + i32 trap-code return; context pointer for store access), the
module/function compile threshold, and whether compiled bodies keep the
`DEFAULT_DEPTH_LIMIT` via an explicit depth counter or a native guard.

## 6. Verification workflow

- Per-cut: `cargo test -p wasm` (unit tests for decode/validation/exec
  edges, floats, traps) plus `cargo run -p wasmtest` over the cut's gate
  files; the runner reports pass/fail/hang per file like the test262 sweep.
- Full-workspace gates after every cut: `cargo clippy -- -D warnings` and
  the native/wasm builds of the embedding demo. Wasm correctness is gated
  by the `wasmtest` CLI corpora plus the standalone `wasm_smoke` example
  (which asserts its output; it is not part of the browser demo).
- Cut 11 has no new suite: its gate is the equivalence harness (see Cut
  11), which must reproduce the interpreter's corpus totals when
  compilation is forced.
- A cut is only "done" when its suite reports **0 failures** and any
  deliberate exclusions have a written taxonomy entry (the test262
  convention), not silent skips.

## 7. Future work (post-baseline, deliberately out of the cuts above)

- **Compilation**: now Cut 11 above; until it lands the interpreter is the
  sole execution path.
- **Threads/shared memory**: `atomic.*` ops wired into the existing
  workers/`Atomics` machinery once multi-agent memory is in place (shared
  memories themselves already work — Cut 10).
- **Web API** (`WebAssembly.instantiateStreaming`/fetch integration,
  `document/web-api`) — a host concern for the browser demo, not the core
  engine.

## 8. Decisions (ratified)

1. **The in-process `wast` crate is the conformance compiler** (wast-tools'
   `wast`, dev/test-only — never a runtime dependency; ratifying the switch
   from wabt 2026-09-05). wabt 1.0.41 cannot parse the pinned corpus's modern
   GC text (rec/sub type defs, packed `i8/i16` storage, `i31ref`, `array
   .init_elem`), and the `wast2json-rs` CLI was unmaintained/problematic;
   embedding the maintained crate removes the external-binary dependency
   entirely. Modules encode in-process (`Wat::encode`); the runner's JSON
   command format is unchanged. The OCaml reference interpreter stays
   deferred until the runner needs it as an oracle.
2. **NaN policy: canonical quiet NaN** for arithmetic results (V8-style),
   payloads only where an operation's semantics require propagating one.
3. **Feature ordering after Cut 5**: exceptions → SIMD/relaxed-SIMD →
   memory64/multi-memory → GC; the JS API (Cut 10) may start in parallel
   once Cut 5 lands.
4. **Naming/wiring**: `crates/wasm` (core) + `crates/wasmtest` (harness);
   `runtime` gains a `wasm` feature with `Context::install_wasm()`, enabled
   by default in the CLI alongside `fs`.
5. **The js-api "crash on `wasm-module-builder.js`" is an engine stack
   budget, not a wasm bug** (2026-09-06): the debug interpreter consumes
   ~160 KB of native stack per JS call level (documented in
   `crates/runtime/src/eval.rs`'s `run_deep`), so the process main thread's
   default 1 MiB stack fits only ~6 JS frames. Verified branch-independent:
   a 7-frame pure-JS chain and the full `cnoop.js` fixture overflow
   identically on `main` and `feature/wasm` debug builds (jit and jitless).
   The upstream fix landed as `6668ffd` (a native-stack recursion guard in
   `crates/runtime/src/stack.rs`): interpreter/JIT activation checks a
   per-thread watermark and throws a catchable RangeError ("Maximum call
   stack size exceeded") instead of letting deep recursion overflow the
   native stack. The guard caps recursion at the native stack size, so the
   js-api harness still runs each fixture on its own 64 MiB-stack thread
   (Wave 5 slice 4): deep-but-finite builder fixtures need the headroom to
   run at all, and runaway recursion surfaces as that fixture's error
   instead of killing the sweep. On targets where the OS cannot report
   stack bounds (e.g. wasm32) `stack_guard_limit()` is `None` and the
   guard is off, so the harness thread matters there too.

## 9. Status

**2026-09-06 — baseline decoder-strictness gaps closed.** The last 18 red
assertions in the top-level `core/` corpus were `assert_malformed` cases the
decoder accepted (`crates/wasm/src/binary.rs`): section payloads that
outlive their declared item count (the `binary.wast` "section size
mismatch" family), a function section with no matching code section, data
count / data section mismatches (incl. `custom.wast`), `memory.init` /
`data.drop` bodies without a data count section, and the 10-byte `i64` LEB
canonicality rule (`binary-leb128.wast`). The decoder now rejects all of
them; the baseline run is **19 969 pass / 0 fail** (was 19 951 / 18) with no
regressions across `exceptions`/`simd`/`multi-memory`/`memory64`/`gc`/
`bulk-memory`. The remaining baseline pendings (600) are `quote text`
converter limitations.

**2026-09-06 — relaxed-SIMD wave lands.** The `relaxed-simd/` corpus (7
files) went from all-pending to **77 pass / 0 fail / 0 pending**: the
relaxed `0xfd` ops (0x100-0x113) decode/validate/execute with one
deterministic legal interpretation each (`simd::exec_relaxed`), and the
runner accepts the `(either …)` result sets. `simd/` stays 25 479 / 0
and the baseline stays 19 969 / 0.

**2026-09-05 — Cut 4 + import follow-up landed, and Cut 5's reference/table
runtime is in** (tables, reference values, `call_indirect`, `call_ref`, bulk
memory). `imports.wast`/`start`/`exports`/`memory_grow` and now
`func_ptrs`/`elem`/`table*`/`call_indirect`/`br_on_null`/`ref_is_null` and
four bulk-memory files are green; `linking.wast` is fully green (163/0).
Remaining: the typed-function-reference *validation* rules (see the Cut 5
entry below), plus converter gaps (`instance.wast`, `ref_null.wast`,
`table_init.wast` GC text).

**2026-09-05 — Cut 2 (validation) green on its gate, plus the module-level
validity files.**

- `crates/wasm/src/valid.rs` implements the spec's algorithmic validator:
  operand + control frames with a `Bot` (unknown) stack entry for
  unreachable-code polymorphism, `select` (implicit num/vec-only, typed
  arity-1), table/global/ref-instruction typing, const-expr rules with the
  per-context visible-global bound, and import/export/element/data checks
  (incl. duplicate-export names, active-segment memory/table existence and
  segment-type ⊑ table-element matching).
- Decoder completions this cut: positional section-order check (tag id 13
  sits positionally between memory and global), tag imports decode the
  `0x00`-attribute byte, element segments match the spec's eight flag
  encodings (funcidx/kind forms type as non-null `(ref func)`; flag-4 items
  as `funcref`), table entries accept the `0x40 0x00`-prefixed initializer
  form.
- `wasmtest` runner: prefers wabt's `wast2json` (env `WAST2JSON`, then
  `wast2json` on PATH, then this machine's wabt build), invoked with
  `--enable-function-references --enable-gc`.
  Deliberately **not** `--enable-all`: it turns on compact-imports, which
  re-encodes every import section and breaks the corpus's standard layout.
- Gate results (0 fail): `type`, `unreached-invalid`, `unreached-valid`,
  `select`, `global`, `elem`, `data`, `imports` (Cut 2 DoD), plus
  `exports`.
- **Excluded from Cut 2 (written taxonomy):** `type-equivalence.wast`,
  `type-rec.wast`, `type-canon.wast` use `(rec …)` type-section text that
  wabt 1.0.41 cannot parse (recursive types + canonicalization are Cut 9).
  Revisit with a GC-capable converter and the Cut 9 rec-type decoder.

### Top-level suite survey (2026-09-05, Cut 3 lead-in)

Ran the whole convertible top-level corpus (91 of 97 files) through the
runner: **3987 pass / 73 fail / 17683 pending** (pending = execution,
linking, and JS-boundary commands — later cuts). Converters cannot parse
6 files (`annotations` text-only, `instance` module-linking-style
instantiation syntax, `ref_null`/`type-*` GC/`(rec)` text).

Systemic fixes landed during the survey:
- Decoder: bounded preallocation from untrusted LEB counts (48 GiB OOM on
  a `binary.wast` malformed fixture); `if`-frame End; no-else-if identity
  rule; function implicit label is a branch target (labels now include the
  function frame); block types embed full `valtype`s (`0x63`/`0x64` forms),
  not just single-byte value types.

Failures by owner:
- **Cut 1 backlog (decoder structural checks):** `binary.wast` (22),
  `binary-leb128.wast` (4), `align.wast` (4), `custom.wast` (1),
  `address.wast` (1) — mostly `assert_malformed` fixtures our decoder
  accepts.
- **Memory/table limits validation:** `memory.wast` (13, limits ≤ 2¹⁶,
  u64/LEB bounds), `table.wast` (9) — Cuts 4/5/8.
- **Typed function references / non-defaultable locals (Cut 5):**
  `br_on_non_null` (3 module bodies), `ref` (5), `ref_func` (2),
  `call_ref` (1), `local_init` (3), `func.wast` uninitialized-typed-local
  (1), `return_call`/`return_call_indirect`/`return_call_ref` (tail-call
  operand typing).

All Cut-3 gate families (`block`, `loop`, `if`, `br`, `br_if`, `br_table`,
`switch`, `return`, `labels`, `nop`, `unreachable`, `local_get/set/tee`)
validate their module bodies — the validator is ready for Cut 3.

### Cut 3 — execution core green (2026-09-05)

- `values.rs`: the value space plus the full ch. 4.3 numeric semantics —
  wrapping integer arithmetic, masked shifts/rotates, div/rem traps,
  truncating and saturating conversions, float ops with spec `min`/`max`
  signed-zero rules, `nearest` ties-to-even, and the canonical-quiet-NaN
  policy (bit-wise `abs`/`neg`/`copysign` preserve payloads).
- `exec.rs`: a flat-pc machine on one shared operand stack with explicit
  function frames (locals + control labels) and a depth-limited call stack,
  so deep recursion (`skip-stack-guard-page.wast`) never touches the host
  stack. Instances own their globals; `instantiate`/`invoke` are exported.
  Unsupported paths (memory/table instructions, reference values, host
  imports, indirect/tail calls) report `ExecFail::Unsupported` so the
  runner counts them *pending*, not failed.
- Runner: `module` commands now instantiate; `register` names modules;
  `action`/`assert_return`/`assert_trap`/`assert_exhaustion` execute and
  compare results, including `nan:canonical`/`nan:arithmetic` expectations
  and the spec trap messages.
- Gate: 0 failures across `i32`, `i64`, `f32`, `f32_bitwise`, `f32_cmp`,
  `f64`, `f64_bitwise`, `f64_cmp`, `conversions`, `const`, `int_exprs`,
  `int_literals`, `float_exprs`, `float_literals`, `float_misc`, `block`,
  `loop`, `if`, `br`, `br_if`, `br_table`, `switch`, `return`, `labels`,
  `nop`, `unreachable`, `stack`, `local_get`, `local_set`, `local_tee`,
  `traps`, `skip-stack-guard-page`. `local_init` stays excluded (typed,
  non-defaultable locals are Cut 5); `traps`' remaining memory cases and
  the `call_indirect`/memory "as-argument" assertions pend on Cuts 4-5.

### Cut 4 — functions, calls, globals, memory, instantiation (2026-09-05)

- `exec.rs` grows linear memory: `Memory { bytes, max_pages }` with
  `memory.size`/`memory.grow` (wasm32 capped at 2¹⁶ pages even without a
  declared max), LE load/store for every width with the extend forms, and
  OOB trapping on effective-address overflow. Active data segments are
  copied at instantiation (before the start function); `instantiate` runs
  the start function. `return_call` replaces the top frame in place at the
  same operand-stack base (tail position reuses the slot, so tail
  recursion does not consume the depth budget).
- Validator/decoder fixes this cut, all driven by the gate:
  - Tail calls validate with subsumption, not equality: a callee may
    produce `(ref null $t)` where `funcref` was declared (`return_call.wast`
    `type-funcref`).
  - Memory limits decode as **u64 LEB** (wabt encodes page counts beyond
    2³² for the `assert_invalid` fixtures) and validation bounds memory32
    at 2¹⁶ pages — imported memory types too (`memory.wast` size limits).
  - Memargs are the post-memory64 form: a single flags byte (low 6 bits
    alignment exponent, bit 6 = explicit memory index, >= 0x80 malformed)
    then a u64 LEB offset; validation rejects offsets > 2³²-1 as "offset
    out of range" and keeps alignment <= natural. `Load`/`Store` offsets
    are `u64` in the instruction model (`align.wast` malformed-flags and
    `offset=2⁶⁴-1` fixtures).
- Runner: quote-based (`.wat`) modules are reported *pending* — the engine
  has no text parser, so those cannot be judged — instead of the previous
  accidental decode-misclassification.
- Gate after the import follow-up below (0 fail unless noted; pending =
  later cuts):

  ```
  func           147 pass /  1 fail / 27 pending   (fail = uninitialized typed local, Cut 5)
  call            88 pass /  0 fail /  3 pending
  forward          5 pass /  0 fail /  0 pending
  fac              8 pass /  0 fail /  0 pending
  start           19 pass /  0 fail /  1 pending
  global          52 pass /  0 fail / 72 pending   (big module has extern/func-ref globals)
  memory          87 pass /  0 fail /  3 pending
  memory_trap    182 pass /  0 fail /  0 pending
  memory_size    42 pass /  0 fail /  0 pending
  memory_grow   102 pass /  0 fail /  4 pending
  memory_redund   8 pass /  0 fail /  0 pending
  load            80 pass /  0 fail / 17 pending
  store           61 pass /  0 fail /  7 pending
  address        259 pass /  0 fail /  1 pending
  align          119 pass /  0 fail / 46 pending   (46 = quote text modules)
  endianness     69 pass /  0 fail /  0 pending
  data            66 pass /  1 fail /  1 pending   (fail = post-memory.init load, Cut 5)
  float_memory   90 pass /  0 fail /  0 pending
  left-to-right  92 pass /  0 fail /  4 pending
  exports        97 pass /  0 fail /  0 pending
  imports       147 pass /  0 fail / 71 pending   (tables/tags/reference globals = Cut 5+)
  linking        70 pass /  1 fail / 92 pending   (fail = Cut 5 table side effects)
  return_call    50 pass /  0 fail /  1 pending
  ```

### Import machinery follow-up (2026-09-05)

The store refactor and host/`register` import resolution landed as a
follow-up, unlocking `imports.wast`, the import halves of `linking.wast`,
`start.wast`'s spectest-start modules, and the register-linked pieces of
`memory_grow`/`exports`/`return_call`.

- `exec.rs` now centers on a [`Store`]: instances share store pools for
  globals and memories, so imported (even mutable) globals and memories
  *alias* the exporter's cells — `linking.wast`'s Mg/Ng shared-mutable-global
  and Mm/Om/Pm shared-memory cases (including growth across instances and
  data-write persistence through failed instantiations) run exactly. Function
  imports flatten to a defined body in some instance or a no-op host
  function (`spectest` `print*`).
- Import type matching: function and global imports compare exactly
  (mutability included); memory imports use the post-memory64 subsumption
  rule with the memory's *current* page count as its minimum (a grown
  memory satisfies a larger import minimum, per `memory_grow.wast`).
- Host functions fix two Cut-4-era gaps exposed by the new traffic: a host
  `call` now advances the caller's pc (it previously re-executed the call
  and trapped on an empty stack), and spectest's `global_f32/f64` are
  `666.6`, not `666`.
- Runner: `Store`-based state with a `(module, field) → ExternVal` registry
  (spectest + `register`-named modules, keyed by both the registered name
  and the `$label` actions use); `assert_unlinkable`/`assert_uninstantiable`
  and `(assert_trap (module …))` instantiation-trap commands judge against
  `InstantiateError`; modules whose instance could not be created are
  tracked as *unavailable* so a later `register`/import of them stays
  pending instead of linking a stale module.
- Written taxonomy (counted, not hidden): `linking.wast` is now fully green
  (163 pass / 0 fail) once tables/elements landed in Cut 5. `func.wast`
  (uninitialized typed local) and `data.wast` (post-`memory.init`) stay Cut 5.
  `instance.wast` cannot be converted: its `(module instance …)` syntax is
  beyond wabt 1.0.41 (module-linking, tracked in the exclusion manifest).

### Cut 5 — runtime: tables, reference values, indirect/ref calls, bulk memory (2026-09-05)

The reference/table runtime landed: `ref` values (`ref.null/func`, host
`ref.extern` payloads, `ref.is_null`, `ref.as_non_null`, `br_on_null/…`),
table cells shared across instances, element segments (active writes +
passive retention for `table.init`/`elem.drop`), `call_indirect` /
`return_call_indirect`, `call_ref` / `return_call_ref` through function
references, the table instructions (`get/set/size/grow/fill/copy/init`,
`elem.drop`), and bulk memory (`memory.copy/fill/init`, `data.drop`) with
per-instance passive data/elem segments.

Gate highlights (0 fail):

```
func_ptrs 36, table 40, table_get 16, table_set 26, table_size 39,
table_grow 58, elem 154, call_indirect 161 (11 pending),
ref_is_null 22, ref_as_non_null 7, br_on_null 10,
bulk: memory_copy 4450, memory_fill 100, memory_init 250,
table_copy 1728, table_fill 45, table-sub 3
```

`linking.wast` became fully green (163/0). Fixes along the way: table
instructions keep their table indices; `table.copy`/`table.fill` typing
(source element ⊑ destination; fill operand order); non-nullable tables
must carry an initializer; `(module definition …)` commands are
validated but never instantiated (wabt writes `definition` as a JSON
string); import matching lets immutable reference globals subsume while
mutable globals and tables stay invariant; the `(ref.func)` result
pattern (wast2json value `0`) means "any non-null function".

**Cut 5 tail closed (typed function references):** the remaining
*validation* rules for typed refs landed and the top-level reference suite
is fully green. New rules:
- **Unknown type indices in inline value types** are rejected wherever a
  typed reference appears: block/loop/if block-type results
  (`resolve_block_type`), `select` result types, elem-segment element
  types, and (with the uninitialized-local work) every function
  param/local (`validate_code` per-local check). `ref.wast` 13/0.
- **`ref.func` requires a *declared* function**: the validator computes
  the module's declared set (elem items of any mode, global/table
  initializers, exports — bodies and `start` declare nothing, matching
  the reference interpreter's `Free.module_` over `funcs = []`, `start =
  None`) and rejects `ref.func` of an undeclared function inside a body.
  `ref_func.wast` 17/0.
- **Uninitialized typed locals**: a non-nullable reference local must be
  `local.set`/`local.tee` before `local.get`; the `Machine` tracks
  per-frame init bits, restoring them at `Else`/`End` so initialization
  inside a structured construct does not escape it. `func.wast`
  (uninitialized local) and `local_init.wast` are green.
- **`br_on_non_null` typing**: the null fall-through consumes the operand
  (it is *not* re-pushed), keeping only the preceding label values; the
  branch carries the operand as the label's final value, which must be a
  reference of the operand's heap type. The executor's null path now also
  drops the operand. `br_on_non_null.wast` 12/0.
- **`call_ref`/`return_call_ref` operands must be *typed* function
  references**: the popped reference is checked against `(ref null
  <type-index>)`, not `funcref`, so a generic `funcref` operand is
  rejected (`call_ref.wast` 35/0, `return_call_ref.wast` 53/0).
- **Trap text**: the runner accepts the spec's suffixed
  `uninitialized element N` (and `undefined element N`) form for the
  index-less engine traps; `bulk.wast` 117/0.

`ref_null.wast` and `table_init.wast` still cannot be converted: their GC
text (`any`/`anyref`, `array.new_default`) is beyond wabt 1.0.41 (and
`instance.wast` needs `(module instance …)`).

### Cut 6 — exceptions: tags, throw, try_table, throw_ref (2026-09-05)

The exception-handling runtime landed. Tags are store cells carrying the
payload function type; `throw` pops a tag's parameters and raises an
in-flight exception that unwinds through call frames to the nearest
matching `try_table` clause (or the invocation edge, where the runner's
`assert_exception` sees it). The engine never uses host unwinding:
`run` delivers exceptions to catch clauses by scanning each frame's
active labels (try scopes are labels over `TryTable` instructions),
dropping inner labels and branching to the clause's target label with
the payload — `catch`/`catch_ref` (payload + exception ref),
`catch_all`/`catch_all_ref`. `exnref` values (`RefValue::Exn`) reference
exception cells in the store; `throw_ref` re-raises one.

Decoder/validation: `throw` (0x08), `throw_ref` (0x0a), `try_table`
(0x1f) with its catch-clause vector, the `exn` heap type (0x69) as
`exnref`, and block types over it. Validation checks each clause's
payload against its enclosing label (tag params, plus the exception ref
for the `catch_ref` forms), rejects unknown tags, and makes `throw`
diverge like `br`/`return`. `assert_exception` is wired into the runner;
tag imports/exports participate in import matching (by payload type).

Gate (0 fail): `exceptions/throw.wast` 13, `exceptions/throw_ref.wast`
15. `imports.wast` improved 194 → 202 pass (the tag import/export
fixtures moved out of pending; still 0 fail). No regressions in the Cut
4/5 sweep.

Two non-obvious points from the runtime: a catch clause's tag immediate
is an *index into the try's instance tag space*, so matching resolves it
to the store cell the thrown exception carries (a naive index-vs-cell
compare only works while cells happen to equal indices — the first
fixture module masks it); and exceptions caught at the *function label*
return the payload as the frame's results, which required the unwind
path to place results exactly like a normal `finish_top`.

`exceptions/tag.wast` (uses `(rec …)` type groups) and
`exceptions/try_table.wast` (newer typed-block catch syntax) cannot be
converted by wabt 1.0.41, so those remain outside the green count; the
engine covers their instructions through the two green files. `tag`
import/export matching by type equality is in place for the JS API
(Cut 10).

Known (pre-existing, not exceptions-specific) validator gap: divergence
inside a *nested empty-result* construct does not mark the enclosing
frame unreachable at its `End`, so `(func (result i32) (block (result
i32) (unreachable)))` validates but wrapping that `unreachable` in one
more empty `(block …)` makes the same function wrongly reject; no green
corpus fixture exercises the rejected shape. A later cleanup should
propagate the unreachable flag outward at `End` instead of faking the
ended frame's result types.

### Cut 7a — SIMD wave 1: substrate + const/lane/memory/bitwise/compare (2026-09-05)

The v128 substrate landed: `Value::V128(u128)` (little-endian lanes, lane
0 low), the runner's v128 const parsing/compare (per-lane, with
`nan:canonical`/`nan:arithmetic` patterns and u64-range lanes), and a new
`simd.rs` semantic module. The 0xfd space decodes: `v128.const`/load/
store, the lane loads/stores, `i8x16.shuffle`/`swizzle`, all splats and
extract/replace lanes, every integer and float comparison, the bitwise
ops (`not/and/andnot/or/xor/bitselect`), `any_true`/`all_true`/`bitmask`,
and the integer shifts and add/sub (i8x16/i16x8/i32x4/i64x2).
`v128.const` is a valid constant expression (globals/instantiation).

Green (0 fail): `simd_const` 577, `simd_lane` 369, `simd_bitwise` 169,
`simd_boolean` 273, `simd_bit_shift` 237, `simd_select` 7, `simd_store`
25, the lane load/store files (52/36/24/16 each), and the comparison
suites `simd_i8x16_cmp` 445, `simd_i16x8_cmp` 465, `simd_i32x4_cmp` 465,
`simd_i64x2_cmp` 113, `simd_f32x4_cmp` 2601, `simd_f64x2_cmp` 2679
(≈6 800 assertions). No regression anywhere else. Two traps found:
8-byte lane masks must not special-case `u128::MAX` (it clears the whole
register on lane-0 replace), and i64x2 comparisons have no unsigned
forms so they map to a 6-op table, not the 10-op table of the smaller
shapes.

**Remaining (Cut 7b)**: the arithmetic families (`*_arith*`, sat arith,
min/max/avgr, narrow, q15mulr, abs/neg/popcnt unops), `*_arith2` files'
extmul/extadd/dot, the conversions (trunc_sat, convert, promote/demote,
`simd_int_to_int_extend`, `simd_splat`), the extend/splat/zero load
forms (`simd_load*`), and `relaxed-simd/`. Most of those files' modules
decode as unsupported today (honest pending); a few `assert_invalid`
modules whose assertions need decoding count as fails until the opcodes
land.

### Cut 7b — SIMD wave 2: arithmetic + conversions (2026-09-05)

The rest of the non-relaxed 0xfd register surface landed in `simd.rs`:
integer arithmetic (wrapping add/sub for all four shapes, i8x16/i16x8
saturating add/sub s/u, i8x16/i16x8 avgr_u, i16x8.q15mulr_sat_s, the
narrow i16x8→i8x16 and i32x4→i16x8 forms, min/max s/u, mul for
i16x8/i32x4/i64x2, abs/neg/popcnt unops, extmul low/high, extadd
pairwise, i32x4.dot_i16x8_s), the float arithmetic (add/sub/mul/div/
min/max/pmin/pmax routed through `values::exec_num` so NaN
canonicalization matches the scalar paths), the float unops
(ceil/floor/trunc/nearest/sqrt/abs/neg for f32x4 and f64x2), the
conversions (trunc_sat f32/f64→s/u, convert i32x4→f32/f64 low forms,
demote/promote), and the load forms (extend 8×8/16×4/32×2, splat
8/16/32/64, zero 32/64) with per-form lane sizing. `valid.rs` types
`VecSig::Unop`/`Not` and accepts `v128.const` in constant expressions;
`exec.rs` runs the whole family through `simd_exec` with a single
load-form reader.

Green across the whole `simd/` corpus (59 files): **25 478 pass,
0 fail, 512 pending** — the pending are the `assert_malformed`
`(module quote …)` fixtures plus the single `multi-memory` module,
which the runner now counts as pending: a plain `module` action whose
decode hits `Error::Unsupported` (a behind-a-cut feature such as
multi-memory, memory64, or GC) is *pending*, not fail (`assert_*`
commands that hit unsupported decode stay fails by design). Green
counts (all 0 fail): arith i8x16 131 / i16x8 194 / i32x4 194 /
i64x2 200; arith2 i8x16 205 / i16x8 170 / i32x4 137 / i64x2 25;
sat i8x16 202 / i16x8 218; q15mulr 30; extadd_pairwise 21+21;
extmul i16x8/i32x4/i64x2 117 each; dot 32; splat 184; the load forms
(load 36, load_extend 98, load_splat 122, load_zero 33);
int_to_int_extend 253; conversions 252; rounding f32x4 185 /
f64x2 185.

Traps fixed this wave:
- **`narrow` loop bounds**: the kernel iterated `16/out_size` times
  over each input but read `in_size`-wide lanes — a 2-byte-input
  narrowing read past the register and shifted a `u128` by ≥128 bits
  (debug panic). It must iterate `16/in_size` input lanes per operand,
  writing output lanes `i` and `i + 16/in_size`.
- **`f64x2.floor` (0x75) missing from `simd::sig`**: its rounding
  siblings 0x74 (ceil), 0x7a (trunc), 0x94 (nearest) were routed, but
  0x75 sat inside the i8x16/i16x8 min/max spans in the real opcode
  table and was dropped, so any module exporting floor decoded as
  unsupported.
- **`vec_load_bytes`/`vec_load_natural` splat widths**: `I16Splat` was
  sized 4 (it reads 2 bytes), false-trapping `load16_splat`/`v16x8`
  loads at the top of a 64 KiB page (addr 65534 + 2 = page end); and
  the validator's natural alignment for the splat/zero load forms used
  8 bytes regardless of form (`load8_splat` align 2, `load16_splat`
  align 4, `load32_splat` align 8 were wrongly accepted). Both tables
  now mirror the true access widths: 1/2/4/8 for 8/16/32/64-bit
  splats, 4 for `load32_zero`, 8 for the widen/`I64Splat`/`I64Zero`
  forms.

### Cut 7c — relaxed SIMD wave (2026-09-06)

The `relaxed-simd/` corpus (7 files) is green: **77 pass, 0 fail, 0
pending**. The relaxed `0xfd` subopcodes (0x100-0x113) decode (added to
`simd::sig` as unary/binary/ternary for typing), validate by category, and
execute with one deterministic legal interpretation each (documented in
`simd::exec_relaxed`): swizzle and q15mulr reuse the MVP kernels,
relaxed trunc aliases the saturating forms, relaxed min/max use the spec
min/max semantics, laneselect is bitselect, madd/nmadd use two-rounding
multiply-then-add, and the dot products multiply signed i8 lanes (i16
saturating / i32 wrapping accumulate). The runner now accepts the
`assert_return` `(either …)` result sets these fixtures use. No
regressions: `simd/` 25 479 pass / 0 fail, the baseline stays 19 969 / 0,
runtime 729.

Still failing (pre-existing, out of Cut 7 scope): the three
GC-`rec`-bag modules — `type-canon` (2), `exceptions/tag` (3), and one
module in `bulk-memory/table_init` (1) — decode the `rec`/subtype
type-section encoding (0x4e/0x50) that the GC binary format (Cut 9)
introduces; today the decoder reports those as malformed functypes.

### Cut 8 — memory64, multi-memory, and table64 (2026-09-05)

The address model landed: every memory instruction now carries its
memory index, memories and tables select an i32 or i64 address type,
and modules may declare any number of memories. The whole `memory64/`
(25) and `multi-memory/` (41) suites are green except the two files the
converter cannot handle.

- **Decoder** (`binary.rs`): memory/table limits flags take bit 0 (max)
  and bit 2 (`i64` address type — memory64/table64); any other flag bit
  is malformed, and the limit values are u64 LEBs. `memarg` returns the
  memory index (the flags byte's bit 6 makes an explicit `memidx`
  follow), threaded through every scalar/vector load/store, lane
  load/store, `memory.size/grow`, `memory.init`, `memory.copy` (dst,
  src), and `memory.fill`. `read_leb` rejects a 10-byte u64 LEB whose
  final byte sets any bit past bit 63.
- **Validation** (`valid.rs`): each memory instruction resolves its
  index to the memory's address type — the address operand and
  `memory.size/grow` are i64 for a memory64, and memarg offsets must fit
  the address type (memory32 offsets are capped at 2^32-1).
  `memory.copy`'s length is the smaller of the two memories' address
  types; `memory.init`'s data offset/length stay i32. The single-memory
  limit is gone, and size caps follow the address type: memories 2^16
  pages (memory32) / 2^48 pages (memory64), tables 2^32-1 / 2^64-1
  slots. The same address-type treatment applies to the table
  instructions (`table.get/set/size/grow/fill/copy/init`,
  `call_indirect`), active data/element segment offsets, and import
  matching (a memory64 only matches a memory64, a table64 a table64).
- **Execution** (`exec.rs`): memory and table cells resolve by index
  across the instance's pools (multi-memory/table instances were always
  vectors), effective addresses are computed at the memory's width
  (i32 bits zero-extended, or i64) with overflow trapping, and
  `memory.size/grow` and `table.size/grow` return the address type. A
  memory64 grows against a 2^48-page ceiling, a memory32 against 2^16;
  a declared maximum always wins for a table64 (a first pass ignored it
  and grew table64s past their max). Active data/element offsets accept
  an i64 const when the target is 64-bit.
- **Runner**: `spectest` gained the spec's `table64` export (a funcref
  table64, 10..20) that `table64.wast` imports.

Green: `multi-memory/` 41/41, and `memory64/` 24/25 — `table_init64`
never converts because wabt 1.0.41 cannot parse the GC `array.new_default`
element items in it (Cut 9, written exclusion). `simd_memory-multi`
(which exercises multi-memory lane forms) flipped from pending to green.
Combined simd + memory64 + multi-memory: 34 153 pass, 0 fail, 570
pending (the pending are `assert_malformed` `(module quote …)`
fixtures). Full sweep of the six convert-with-wabt suites (core root,
exceptions, bulk-memory, simd, memory64, multi-memory — 234 files):
224 run, 61 309 pass, 20 fail, 1 168 pending; the 20 fails are
pre-existing decode gaps outside this cut (`binary.wast` 15 structural
malformed checks, `binary-leb128.wast` 4 `i64.const` canonicality,
`custom.wast` 1 data-count decode check). Ten files never convert under
wabt (annotations/instance/ref_null/`type-*`/tag/try_table/table_init/
table_init64 — GC rec bags, GC elem items, or new module-linking
syntax); each is listed in the runner's exclusion manifest.

#### Cut 8 taxonomy and CI gate

`wasmtest run` now accepts several paths and walks directories
recursively for `.wast`/`.json`, converting on the fly and exiting
nonzero when any suite reports a fail — the CI-style gate. Files the
pinned converter cannot parse live in `crates/wasmtest/wasm-exclusions.txt`
(`path :: reason`, overridable via `$WASM_EXCLUSIONS`); they print as
`skip` and are not failures. The gate for this cut:

```
wasmtest run waspec/test/core/multi-memory waspec/test/core/memory64
# total: 8674 pass, 0 fail, 59 pending, 1 skipped
```

| suite | file | pass / fail / pending |
|---|---|---|---|
| memory64 | address64 | 242 / 0 / 0 |
| memory64 | align64 | 111 / 0 / 46 |
| memory64 | binary_leb128_64 | 2 / 0 / 0 |
| memory64 | bulk64 | 70 / 0 / 0 |
| memory64 | call_indirect64 | 2 / 0 / 0 |
| memory64 | endianness64 | 69 / 0 / 0 |
| memory64 | float_memory64 | 90 / 0 / 0 |
| memory64 | load64 | 85 / 0 / 13 |
| memory64 | memory64 | 69 / 0 / 0 |
| memory64 | memory64-imports | 78 / 0 / 0 |
| memory64 | memory_copy64 | 4450 / 0 / 0 |
| memory64 | memory_fill64 | 100 / 0 / 0 |
| memory64 | memory_grow64 | 49 / 0 / 0 |
| memory64 | memory_init64 | 250 / 0 / 0 |
| memory64 | memory_redundancy64 | 8 / 0 / 0 |
| memory64 | memory_trap64 | 172 / 0 / 0 |
| memory64 | table64 | 14 / 0 / 0 |
| memory64 | table_copy64 | 1728 / 0 / 0 |
| memory64 | table_copy_mixed | 4 / 0 / 0 |
| memory64 | table_fill64 | 80 / 0 / 0 |
| memory64 | table_get64 | 11 / 0 / 0 |
| memory64 | table_grow64 | 22 / 0 / 0 |
| memory64 | table_init64 | skip — GC `arrayref` element items (Cut 9) |
| memory64 | table_set64 | 19 / 0 / 0 |
| memory64 | table_size64 | 37 / 0 / 0 |
| multi-memory | address0 | 92 / 0 / 0 |
| multi-memory | address1 | 127 / 0 / 0 |
| multi-memory | align0 | 5 / 0 / 0 |
| multi-memory | binary0 | 7 / 0 / 0 |
| multi-memory | data0 | 7 / 0 / 0 |
| multi-memory | data1 | 14 / 0 / 0 |
| multi-memory | data_drop0 | 11 / 0 / 0 |
| multi-memory | exports0 | 8 / 0 / 0 |
| multi-memory | float_exprs0 | 14 / 0 / 0 |
| multi-memory | float_exprs1 | 3 / 0 / 0 |
| multi-memory | float_memory0 | 30 / 0 / 0 |
| multi-memory | imports0 | 8 / 0 / 0 |
| multi-memory | imports1 | 5 / 0 / 0 |
| multi-memory | imports2 | 20 / 0 / 0 |
| multi-memory | imports3 | 10 / 0 / 0 |
| multi-memory | imports4 | 16 / 0 / 0 |
| multi-memory | linking0 | 6 / 0 / 0 |
| multi-memory | linking1 | 14 / 0 / 0 |
| multi-memory | linking2 | 11 / 0 / 0 |
| multi-memory | linking3 | 14 / 0 / 0 |
| multi-memory | load0 | 3 / 0 / 0 |
| multi-memory | load1 | 18 / 0 / 0 |
| multi-memory | load2 | 38 / 0 / 0 |
| multi-memory | memory-multi | 6 / 0 / 0 |
| multi-memory | memory_copy0 | 29 / 0 / 0 |
| multi-memory | memory_copy1 | 14 / 0 / 0 |
| multi-memory | memory_fill0 | 16 / 0 / 0 |
| multi-memory | memory_grow | 51 / 0 / 0 |
| multi-memory | memory_init0 | 13 / 0 / 0 |
| multi-memory | memory_size0 | 8 / 0 / 0 |
| multi-memory | memory_size1 | 15 / 0 / 0 |
| multi-memory | memory_size2 | 21 / 0 / 0 |
| multi-memory | memory_size3 | 2 / 0 / 0 |
| multi-memory | memory_size_import | 7 / 0 / 0 |
| multi-memory | memory_trap0 | 14 / 0 / 0 |
| multi-memory | memory_trap1 | 168 / 0 / 0 |
| multi-memory | start0 | 9 / 0 / 0 |
| multi-memory | store0 | 5 / 0 / 0 |
| multi-memory | store1 | 13 / 0 / 0 |
| multi-memory | store2 | 25 / 0 / 0 |
| multi-memory | traps0 | 15 / 0 / 0 |

All 59 pending across the gate are `assert_malformed (module quote …)`
text fixtures (the engine has no `.wat` parser); the single skip is the
documented `table_init64` converter exclusion. The exit code is nonzero
the moment any suite reports a fail, so the command doubles as the cut's
CI check.

### Cut 9 — GC: converter switch + engine (in progress, 2026-09-05)

The converter switch (ratified above) landed first and is complete: the
in-process `wast`/`wat` crates (default-features off, `wast` with
`wasm-module`) convert every `.wast` in the pinned corpus to the
runner's wast2json-shaped JSON + per-module `.wasm`/`.wat` files,
replacing wabt's `wast2json` (which cannot parse GC text). `WAST2JSON`
still forces wabt; the converter is exercised as the default in every
suite run. The `wast` tokenizer's NaN-bit resolution matches wabt's, so
converted float/v128 values keep bit parity.

#### Cut 9 Wave 1 — GC type model + decoder (landed)

- `types.rs`: full heap-type hierarchy (`any`/`eq`/`i31`/`struct`/`array`
  and bottoms `none`/`nofunc`/`noextern`/`noexn`), storage types (packed
  `i8`/`i16`), `FieldType`, `CompositeType` (func/struct/array), and
  `SubType` (final flag, supertypes). `Module.types` is now a
  `Vec<SubType>` (a rec group occupies consecutive indices); func-type
  resolution goes through `Module::func_at`.
- `binary.rs`: the type section decodes rec groups (`0x4e`), `sub`/`sub
  final` subtypes (`0x50`/`0x4f` + supertype list + composite), struct
  (`0x5f`) and array (`0x5e`) composite types, packed storage, GC value
  types (single-byte abstract-heap shorthands and `0x63`/`0x64` typed
  refs), and s33-encoded heap types. Block types, table elements, and
  fields all accept the GC forms.
- Decoder tests round-trip struct/array/sub/rec/gc-valtype encodings.
- Result: every `gc/` fixture now *decodes and passes module
  validation*; `type-canon.wast` fully passes, and the pre-Cut-9
  exclusions (`type-rec`, `type-equivalence`, `ref_null`, GC `table_init`
  fixtures, `exceptions/tag`) decode cleanly and validate against the
  engine (only genuine GC typing gaps remain as failures). No regression
  elsewhere: the non-GC suites keep their prior totals.

#### Cut 9 Wave 2 — GC instruction decode + validation typing (landed)

- `instr.rs`: the full GC aggregate/cast instruction surface (`0xfb`
  prefix): `struct.*`, `array.*` (incl. `new_fixed`/`new_data`/`new_elem`,
  `fill`/`copy`/`init_data`/`init_elem`), `ref.test`/`ref.cast`/
  `br_on_cast`/`br_on_cast_fail`, `ref.i31`/`i31.get_*`, and
  `any.convert_extern`/`extern.convert_any`.
- `binary.rs`: decodes the `0xfb` prefix (subopcode + immediates, cast
  flags for the branching casts).
- `valid.rs`: context-aware GC subtyping replaces the old context-free
  reference matching: `heap_sub`/`abs_sub`/`ref_sub`/`val_sub` cover the
  abstract heap lattice, concrete types under their composite kind's
  abstract type, and declared supertype chains; `Machine` carries the
  module types. Every GC instruction is typed per spec (aggregate field
  mutability/packing rules, `array.new_elem`/`init_elem` element-type
  subsumption, `array.copy` storage compatibility, cast targets within
  the same hierarchy, `ref.eq` limited to `eq`-comparable operands, the
  `br_on_cast` label/difference typing). `valid::Error::Unsupported` was
  added and the runner counts it as pending, so modules using a feature
  validation does not yet judge stay honest.
- Non-GC suites keep identical totals (19949/18 top-level core, zero-fail
  feature dirs); gc fixtures now classify against the typed validator
  (assert_invalid and structure tests pass; execution is the next wave).

Remaining Cut 9 engine wave: a GC object runtime in the store
(struct/array/i31 objects, extern wrappers), the executor for the
`0xfb` instructions (incl. data/elem segment access and cast
semantics), and the `gc/` file sweep — against `gc/` (17).

### Quote-text converter + reserved-opcode decode (2026-09-06)

The runner no longer marks every `assert_malformed`/`assert_invalid`
`(module quote …)` fixture pending. `wasmtest` now:

- parses each quote sidecar (`.wat`) with the `wast` crate and encodes
  it to binary (`wat_text_to_binary`, trying a `(module …)` wrapper
  first, then bare); for `assert_malformed` a text/encode parse failure
  or a binary-level malformed decode is a pass, a decode that succeeds
  is a fail; for `assert_invalid` a parse failure is a fail, an
  invalid/valid verdict comes from the validator, and decode/validation
  `Unsupported` stays pending.
- `binary.rs` classifies the reserved single-byte opcode gaps as
  malformed rather than unsupported, matching the pinned spec's
  interpreter (`illegal opcode`): legacy EH `try`/`catch`/`rethrow`
  (0x06/0x07/0x09), the 0x16-0x19, 0x1d-0x1e, 0x27, 0xc5-0xcf,
  0xd7-0xfa control/numeric gaps, and 0xfe/0xff. This clears the last
  baseline pendings (`binary.wast` elem/body reserved-opcode fixtures)
  and the `exceptions/try_table.wast` legacy-`catch` fixtures (the
  `wast` crate still parses the old standalone `catch`/`catch_all`
  instruction forms, which encode to bytes the spec reserves).

Measured (2026-09-06, in-process converter, single suite dirs):

| Suite | pass / fail / pending / skip |
|---|---|
| baseline `core/*.wast` | 20,569 / 0 / 0 / 6 |
| `exceptions/` | 95 / 0 / 0 / 1 |
| `simd/` | 25,990 / 0 / 0 / 0 |
| `relaxed-simd/` | 77 / 0 / 0 / 0 |
| `multi-memory/` | 912 / 0 / 0 / 0 |
| `memory64/` | 8,709 / 0 / 0 / 0 |
| `gc/` | 654 / 0 / 0 / 1 |
| `bulk-memory/` | 7,485 / 0 / 0 / 0 |

Every suite that previously carried quote-text pendings (baseline 600,
`simd` 511, `memory64` 59, `exceptions` 2, `gc` 1) now reports zero
pendings. Remaining skips are documented Cut 9 converter exclusions
(`type-rec`, `type-equivalence`, GC multi-supertype text the `wast`
grammar rejects) and one `gc` fixture.

### GC type-lattice bottoms + null `exnref` (2026-09-06)

`ref_null.wast` is green and left the exclusions. Two engine gaps and
one runner gap:

- `valid.rs`: hierarchy bottoms now subsume every concrete type of
  their hierarchy (`nofunc` under any defined function type, `none`
  under any defined struct/array type, spec 3.3.3), and `none` is
  below `i31` — it is the bottom of the whole aggregate hierarchy
  (spec: "common subtype of all forms of aggregate types"). The unit
  test that asserted `none` and `i31` are siblings was spec-wrong and
  is corrected.
- `wasmtest`: `matches_expected` treats an `exnref` expectation like
  the other reference kinds (a null expectation matches a null
  reference; `ref.null exn` returned as a result was previously
  unmatched, falling through to the numeric-value path).

Measured: baseline `core/*.wast` 20,603 / 0 / 0 / 5 skip
(`ref_null.wast` now runs). The remaining core exclusions are the
isorecursive rec-group canonicalization wave (`type-equivalence`,
`type-rec`, `exceptions/tag`), the `wast`-grammar multi-supertype limit
(`gc/type-subtyping`), and harness tooling (`annotations`/`instance`/`names`).

### Isorecursive rec-group type equivalence (2026-09-06)

The last engine-gap exclusions closed. `Module` now records its type
section's rec-group lengths (`rec_groups`), and type equality is the
spec's isorecursive equivalence: two type indices are equal iff their
rec groups have equal length and are structurally identical
member-for-member, with in-group references mapping by offset and
references to earlier (closed) groups comparing the referenced types
(`crates/wasm/src/module.rs`). This makes two *separate* but isomorphic
rec groups — including cross-module ones at link time — denote the same
type, while non-isomorphic groups (same shape, different member count
or alignment) stay distinct.

Where it plugs in:
- `valid.rs`: reference subsumption (`heap_sub`) treats equivalent
  types as equal before following declared supertype edges, and a type
  definition's references are scoped to its rec group (`unknown type`
  for a forward reference into a later group). The subtyping helpers
  now take the whole `Module`.
- `exec.rs`: function imports, tag imports, and the indirect-call /
  `call_ref` runtime checks compare across the two modules' type spaces
  by equivalence (tags record their declaring instance); runtime
  `ref.cast`/`ref.test` concrete targets compare across modules too.
- `binary.rs`: rec-group lengths are recorded during type-section
  decode.

Measured: baseline `core/*.wast` 20,662 / 0 / 0 / 3 skip;
`exceptions/` (incl. `tag.wast`) 105 / 0 / 0; `gc/` 654 / 0 / 0 with the
single remaining skip the `wast`-grammar multi-supertype file
(`gc/type-subtyping`). Remaining core skips are harness tooling
(`annotations`/`instance`/`names`) and that one converter limit. The
JS-API corpus is unchanged at 1001 / 0.

### Conformance baseline complete; Cut 11 defined (2026-09-06)

The in-scope conformance surface is done: the core suite (baseline +
all proposal dirs) and the JS-API corpus report zero failures and zero
pendings, and no engine-gap exclusions remain — the exclusions file now
holds only harness tooling, out-of-scope proposals, and the one
`wast`-grammar limit. Housekeeping: the stale `exec.rs` module header
(GC no longer unimplemented) and the `wasm-exclusions.txt` header were
updated. Cut 11 (Compilation: wasm-to-native via Cranelift) was added to
section 5 as a planned, not-started performance cut with an equivalence
harness as its gate; the matching future-work bullet in section 7 was
reworded.

### Cut 10 exit: standalone WebAssembly smoke (2026-09-06)

The final Cut 10 exit item landed as a standalone example, not in the
browser demo. `wasm_smoke`'s default self-test compiles, instantiates,
and calls a wasm module from JS (`add(20, 22)` → 42) and reads a
`WebAssembly.Memory`/`Global`; `cargo run -p slag --example wasm_smoke`
asserts the output and exits non-zero on any mismatch. The browser demo
pages (`browser/demo.html`, `docs/index.html`) and `wasm_binding` were
left untouched — wasm correctness is already verified by the `wasmtest`
CLI corpora, and the dogfood UI is for the JS engine, not a wasm
showcase.

### Cut 11 planning: wave breakdown written; Wave 0/1 next (2026-09-06)

Cut 11's scope section now carries a wave breakdown (substrate +
equivalence gate → numeric leaf core → memory/globals/tables → calls
and the host boundary → traps/exceptions/refs/GC/SIMD) grounded in the
interpreter's actual model: flat `Vec<Instr>` stack machine, store-pool
frames/instances, resumable host calls, precise NaN/trap semantics (the
equivalence harness oracle). The compiler will live in `crates/wasm`
behind an optional `compile` feature (cranelift 0.134.3, the workspace-
pinned version), not in `crates/jit` (bound to the JS `Step` VM). Wave 0
wires the gate (`wasmtest` compile-forced equivalence mode); Wave 1
lowers the numeric leaf core (const/num/local/structured control/
return) with a trap-code-returning entry ABI.

### Cut 11 Wave 0/1 first slice landed (2026-09-06)

The substrate and the first lowering slice are in (feature `compile`,
off by default):
- `crates/wasm` gains the optional `compile` feature and cranelift
  0.134.3 deps; `lib.rs` gates `pub mod compile` on it.
- `compile.rs`: the u64-slot entry ABI (`fn(*const u64, u64, *mut u64,
  u64) -> i32`, trap code 0 = ok), `run_compiled` marshaling
  interpreter `Value`s, `compile_module`, a cached native `Engine`, and a
  Cranelift lowering for the integer leaf subset: `const`, `local.*`,
  `drop`, `select`, `unreachable`, `return`, and the i32/i64 `Num`
  arithmetic/comparison/shift/popcnt ops plus `i64.wrap_i32`. Traps
  (`unreachable`, integer div-by-zero, `MIN/-1` overflow) return codes
  through guarded branches — no Cranelift `trap`. `MIN % -1` is 0 per
  spec (the divisor is masked to 1 for `srem`). Floats, structured
  control, memory, globals, tables, calls, refs, SIMD, and GC are not
  lowered yet (whole body bails to the interpreter).
- `exec.rs`: `Instance` carries `compiled: Vec<Option<CompiledFunc>>`
  (parallel to `Module::bodies`, populated at instantiation), and
  `start_owned` runs the compiled entry when present — synchronous and
  never resumable in this subset, so it returns `Finished` directly.
- Gate so far: six unit tests drive modules through the store and check
  results and trap kinds (add, select, div traps, rem `MIN%-1`, and a
  call-bearing function falling back to the interpreter). `cargo test -p
  wasm` 36 pass default / 42 with `--features compile`; clippy clean in
  both configurations. The corpus-wide `wasmtest` compile-forced
  equivalence mode is the next Wave 0 item.

Follow-up increment: a store-level interpreter-forcing toggle
(`Store::set_compile(false)`, feature-gated) turns the unit tests into a
real equivalence harness (`assert_equiv` compares compiled vs
interpreter outcomes bit-for-bit, values and trap kinds, over the same
store path). Coverage now sweeps every lowered op over edge input sets:
all i32/i64 binary arithmetic/comparison/shift ops (shifts and rotations
mask their count explicitly, never trusting the backend), `popcnt`,
`eqz`, `i64.wrap_i32`, `select`, and locals/tee — 10 compile tests, 46
total
with the feature, clippy clean in both configurations. Remaining Wave 1
work: structured control flow (`block`/`loop`/`if`/`br*`), then floats
with exact canonical-NaN semantics, before the corpus-wide gate.

Structured control flow landed: the lowering driver is now a real
stack-to-CFG translator. Blocks, loops, and if/else lower to sealed
Cranelift blocks whose continuations carry the construct's (parameter-
free, single-result) blocktype results as block parameters; loop headers
are sealed at their `end` once every back-edge predecessor is declared
(a predecessor may only target an unsealed block), so the variable
machinery inserts the loop-carried phis. `br`/`br_if` target the
continuation (block/if, carrying results) or the header (loop); dead
regions after unconditional branches skip codegen while tracking
structure depth, and continuations with live branch predecessors are
filled on close. Locals/operand stacks stay SSA-correct across joins via
block params. Equivalence tests now cover blocks with values, `br_if`
exits, if/else results, a back-edge loop, a loop that falls out of its
`end`, and a nested if/else-in-loop (Collatz) — 16 compile tests, 52
total with the feature, clippy clean in both configurations. Not yet
lowered: `br_table`, parameterized/multi-value block types, and floats
(next, with exact canonical-quiet-NaN semantics).

Floats landed: `clif_type` now covers `f32`/`f64`, so float
params/results/locals ride the same SSA value model (arguments still
arrive as u64 slots — low 32 bits for f32, full 64 for f64 — bitcast to
IEEE Cranelift values). `lower_float` handles the f32/f64 unops, binops,
comparisons, and int→float conversions; arithmetic/rounding/sqrt results
are NaN-canonicalized to the engine's QNAN32/QNAN64 (`fcmp != self`
`select`s in the canonical constant), reproducing the interpreter's
canonical-quiet-NaN policy bit-for-bit, while `abs`/`neg`/`copysign`
stay raw bit ops (`fabs`/`fneg`/`fcopysign`) and conversions lower via
`fcvt_from_sint`/`uint`. `f32.min/max` and promote/demote stay deferred.

The equivalence harness is now honest: `assert_equiv` first asserts the
module actually compiled (`Engine::compile` degrades to the interpreter
silently on any lowering error), so a test can no longer compare the
interpreter against itself. That gate exposed three previously vacuous
modules — two declared `i64`-producing ops (arithmetic, `popcnt`) with
an `i32` result type and one `select`/locals module whose stack
underflowed — now corrected to valid wasm. New coverage sweeps f32/f64
binary ops, comparisons, unary ops (incl. rounding/sqrt), and int→float
conversions over signed-zero, subnormal, ∞, and quiet/signaling-NaN
inputs, plus float values flowing through block and if/else results and
untyped float `select`. 22 compile tests, 58 total with the feature,
clippy clean in both configurations.

`br_table` and the remaining bit-counting ops landed, closing out the
Wave 1 numeric/control list: `do_br_table` pops the `i32` index and
emits a chain of `index == k` guards, each branching to its label's
target with the shared payload — block/if labels jump to the
continuation carrying the construct's result, loop labels jump to the
(until-then-unsealed) header — ending in an unconditional jump to the
default. The interpreter's negative-index rule (any index < 0 selects
the default) falls out of the signed equality guards. `I32Clz`/`I32Ctz`
and `I64Clz`/`I64Ctz` lower to Cranelift's `clz`/`ctz` (defined at
zero, matching wasm). Equivalence coverage adds a same-target payload
block, a dispatch across three nested value blocks (each exit carries
the payload through a different post-processing chain), a dispatch
across empty blocks returning per-case constants, a sum loop whose exit
is a `br_table` case targeting the unsealed loop header, and `clz`/
`ctz` over the edge integer sets (zero included). 23 compile tests, 59
total with the feature, clippy clean in both configurations. Remaining
Wave 1 gap: parameterized/multi-value block types (block parameters and
several results), which need block-parameter plumbing on construct
entry.

Parameterized and multi-value block types landed, completing the Wave 1
numeric/control leaf list. Block types now resolve through the module's
type section (`block_sig`, so `BlockType::Type(i)` carries any numeric
parameter/result list), and constructs model the wasm discipline
exactly: a block type `[t1*] -> [t2*]` enters with `t1*` already on the
operand stack — the label base (`height`) moves below them and the body
starts from them; a `loop` jumps to a header whose block parameters ARE
the first iteration's `t1*`, and `br` to its label carries `t1*` back
(its label arity is the parameter count); block/if labels keep their
result arity and jump to the continuation; an `if` saves its parameters
so the else branch restarts from the same values (an `if` with
parameters but no else stays interpreted). `br`/`br_if`/`br_table` share
one `frame_target` label resolver, so `br_table` over parameterized
loop labels and multi-value block results lowers too. Equivalence
coverage adds a parameterized block (body adds 2 to its input), a
multi-result `(i32 i32)` block, an `if` whose parameter feeds both
branches, single- and two-parameter loops, and a loop whose `br_if`
back-edge carries its parameter until the exit condition — all matched
bit-for-bit against the interpreter. 24 compile tests, 60 total with
the feature, clippy clean in both configurations. Wave 1's leaf list is
done; the next step is the Wave 0 gate: the corpus-wide compile-forced
equivalence harness in `wasmtest`.

Wave 0's equivalence gate landed in `wasmtest`: `wasmtest equiv
<wast|json|dir>` runs each suite twice — once through compiled bodies,
once with the interpreter forced (`Store::set_compile(false)`) — and
fails on the first command whose verdict (or command type) diverges.
`run_json` was parameterized into `run_json_mode(path, compiled,
verbose)`, returning a per-command (command type, verdict) log for the
comparison; `run` keeps its interpreter-forced behavior, so the totals
are unchanged. The runner's `wasm` dependency enables the `compile`
feature. Supporting changes in `crates/wasm`: the native `TargetIsa` is
now built once per process (`OnceLock` in `native_isa`) instead of per
module instantiation, and an interpreter-forced store skips the compile
pass entirely (the harness instantiates whole corpora twice). The gate
is green over the numeric/control core suites (`i32`/`i64`/`f32`/`f64`
and bitwise/cmp, `const`, `block`, `loop`, `if`, `br*`, `select`,
locals, `switch`, `return`, `nop`, `labels`, `stack`, `int_exprs`/
literals, `float_exprs`/literals/misc, `conversions`, `fac`, and the
bitwise float files: 30 suites, 0 diverged). Compiled coverage is the
Wave 1 leaf subset, so most corpus bodies still run interpreted on both
sides; later waves (memory, calls, …) widen what the gate actually
exercises.

Wave 2's first slice landed: numeric loads/stores and `memory.size`
over a module's single 32-bit memory (index 0). The compiled-entry ABI
grew a memory data pointer + byte length pair (`fn(args, nargs, mem,
mem_len, out, nout) -> i32`), fetched per call from the instance's
memory cell — no store access from generated code, and safe because the
leaf subset never grows memory mid-call (growth stays interpreted).
`mem_ea` bounds-checks `addr + offset + width` against the length and
traps (`TRAP_MEMORY_OOB` → `OutOfBoundsMemoryAccess`) inline, then
loads/stores little-endian at the effective address: every numeric
width with sign/zero extension (`i32.load8_s/u` … `i64.load32_s/u`,
byte/half stores), float loads/stores as raw bits, and `memory.size` as
pages. Functions touching other memory indices, memory64, or growth
still fall back to the interpreter. The equivalence harness caught a
real bug immediately: `mem_ea` returned `base + addr` and dropped the
static offset, so offset loads read the wrong bytes — the corpus
divergence (`address.wast`) localized it to the missing offset, and the
unit test now uses offset-vs-offset-0 cross-checks that cannot hide a
dropped offset. Unit coverage sweeps every store/load width over the
edge value sets, OOB traps, non-zero offsets (both directions), float
bit round-trips, and `memory.size`. `wasmtest equiv` is green over the
memory suites (`address`, `align`, `endianness`, `float_memory`, `left-
to-right`, `load`, `memory`, `memory_redundancy`, `memory_size`,
`memory_trap`, `store`: 11 suites, 0 diverged). 25 compile tests, 61
total with the feature, clippy clean in both configurations. Wave 2
remaining: `memory.grow`, memory64/multi-memory, globals, and the
table/`call_indirect` family.

Globals landed next: `global.get`/`global.set` of numeric module-defined
globals lower through a caller-owned `gvals` buffer (one u64 slot per
used global, entry ABI param 4, slot order = sorted used indices). The
compiled body loads and stores slots in place, so a write is visible to
the store even when a later instruction traps (the caller copies the
buffer back to the store's cells after the call, success or trap) —
wasm traps do not roll back prior writes. Only module-defined globals
compile: an imported global's cell can alias another import (the same
provider global imported twice), which the buffer model cannot express;
import-touching bodies stay interpreted. The `lowerable` gate requires
numeric defined globals (`global.set` additionally mutable), the entry
reads current cell bits into the buffer before the call and writes the
buffer back after. Unit coverage: immutable reads, i32/i64/f32/f64
set-then-get round trips, a set-before-`unreachable` function whose
write must persist to a following getter (cross-call), all through both
the per-call equivalence harness and a new ordered `run_seq` helper.
`wasmtest equiv` stays green over `global.wast`, `imports.wast`, and the
memory/numeric regressions. 26 compile tests, 62 total with the
feature, clippy clean in both configurations. Wave 2 remaining:
`memory.grow` (needs store-side realloc), memory64/multi-memory, and the
table/`call_indirect` family.

Multi-memory loads/stores/size landed: the snapshot `mem`/`mem_len`
ABI pair became a per-call memory descriptor array (one (data pointer,
byte length) `u64` pair per module memory index, entry ABI params 2/3),
so compiled memory ops address any memory, not just index 0. `mem_ea`
and `memory.size` reload the descriptor for the instruction's memory
index at each access; the `lowerable` gate now resolves each used
memory's type through the index space (imported memories first) and
accepts it when not memory64. Growth still keeps a function
interpreted (it would reallocate the buffer the descriptors snapshot),
and memory64 addressing stays interpreter-side. Unit coverage adds a
module with a 1-page and a 2-page memory storing/loading distinct
values into each and summing the `memory.size`s, so a descriptor mixup
diverges. `wasmtest equiv` is green over the whole `multi-memory` suite
(41 suites) and `memory64` (25 suites, 0 diverged there as expected —
memory64 bodies stay interpreted). 27 compile tests, 63 total with the
feature, clippy clean in both configurations. Wave 2 remaining:
`memory.grow` (needs a store-side realloc helper + descriptor
refresh), memory64 addressing, and the table/`call_indirect` family.

`memory.grow` landed, the last 32-bit memory gap before the 64-bit and
table work. The entry ABI grew to `(args, nargs, mems, ncount, gvals,
out, nout, store, instance, grow)`: params 7-9 carry the owning store
pointer, the invoked instance index, and the code address of a Rust grow
helper (`exec::memory_grow_helper`, an `extern "C"` fn reached from
generated code through a helper-signature `call_indirect` whose params
are all u64 slots, so no float classification can disagree).
`do_memory_grow` widens the popped i32 delta, computes the memory's
descriptor-slot address (`mems + 16*memory`), and calls the helper with
store/instance/memory-index/delta/desc-slot; the helper resolves the
module memory index to the instance's store cell, grows the backing
`Vec` through `Store::grow_memory` (enforcing the cell's declared max
and the memory32 2^16 cap), and on success rewrites the descriptor
entry's data pointer + byte length in place — a `Vec` data pointer is
unstable across a resize, and every later `mem_ea`/`memory.size` reloads
the descriptor, so the growth is visible to subsequent compiled
accesses. The helper returns the old page count or all-ones (-1); the
generated code narrows it to i32, reproducing wasm's "old pages or -1"
exactly (32-bit growth caps far below 2^32). `lowerable` now admits
`memory.grow` on any 32-bit memory index (multi-memory growth compiles
too); memory64 stays interpreter-side. Unit coverage grows both an
unbounded memory (pre-grow store, grow to 2 pages, stores on the new
page and its last aligned slot, cross-call reads back all three values
via `run_seq` — a stale descriptor after the `Vec` realloc would write
through freed memory) and a `(1 2)`-bounded memory (grow-to-max returns
old pages × new size, the next grow fails with -1, and a page-boundary
store/load after the successful grow lands). `wasmtest equiv` is green
over `memory_grow.wast` (106 commands agree) and the memory +
`multi-memory` regression sweep (41 suites), 0 diverged. 29 compile
tests, 65 total with the feature, clippy clean in both configurations.
Wave 2 remaining: memory64 addressing and the table/`call_indirect`
family.

Memory64 addressing landed: compiled memory ops now accept any memory
(32- or 64-bit) and resolve each instruction's address-operand width
per its memory index, so one function can touch a memory32 and a
memory64 side by side. `mem_ea` keeps the memory's index type: a memory32
address is zero-extended from the i32 operand; a memory64 address is the
i64 operand as-is, with the offset/width adds wrapping mod 2^64 and each
carry trapping as OOB — the interpreter's `checked_add` semantics
(an access whose end passes 2^64 is out of bounds whatever the wrapped
bits look like). `memory.size`/`memory.grow` return the memory's index
type too (i64 for a memory64: i64 delta in, old-pages-or-i64-(-1) out),
and growth caps follow the cell's address type through the store helper.
The equivalence gate immediately caught a real bug in this slice: the
refactored `mem_ea` returned the access's *end* (`base + ea + width`)
as the pointer instead of its start, so every store/load landed one
access-width past its address — symmetric store-then-load unit tests
masked it (both shifted together), while the `memory64` corpus exposed
it (wrong values, out-of-bounds reads). Unit coverage adds a memory64
store/load/size module over in-bounds and huge addresses (including
i64::MAX, i64::MIN, and −8 whose 8-byte width carries past 2^64), a
bounded memory64 grow sequence, and a mixed memory32+memory64 module.
`wasmtest equiv` is green over `memory64/` (25 suites, 0 diverged — now
exercising compiled memory64 bodies, not interpreter-vs-interpreter) and
the memory + `multi-memory` regressions. 32 compile tests, 68 total with
the feature, clippy clean in both configurations. Wave 2 remaining: the
table/`call_indirect`/`ref.func` family (needs the compiled value model
extended to refs and native↔interpreter function re-entry).

Compiled `call`/`call_indirect`/`return_call`/`return_call_indirect`
landed — the native↔interpreter function-boundary re-entry that
unlocks non-leaf bodies. The entry ABI grew to `(args, nargs, mems,
ncount, gvals, out, nout, store, instance, grow, call, scratch)`: params
10-11 carry the call helper's address and a caller-owned u64 scratch
region (`SCRATCH_SLOTS`, sized so any oversized call site stays
interpreted). A call site spills its numeric params to `scratch[0..n)`,
calls the helper through a helper-signature `call_indirect`, and loads
the results back from `scratch[0..m)`; a nonzero trap-code return exits
the entry verbatim. The helper (`exec::wasm_call_helper`) resolves the
target exactly as the interpreter would (direct: the instance's func
index space; indirect: table element with bounds/null checks and the
`IndirectCallTypeMismatch` type check), decodes args by the declared
type, and runs the callee to completion *through the interpreter* with
`compile_off` forced — identical semantics, a bounded native stack (no
compiled-to-compiled native recursion), and the interpreter's own
frame-depth/host-boundary behavior. Non-trap errors park the exact
`ExecFail` on the store and return a `TRAP_PENDING_ERROR` sentinel that
`run_compiled` drains, so an escaping exception or external-host
boundary surfaces identically to an interpreted run. After the callee
runs, the helper refreshes the caller's memory descriptors in place (an
interpreted callee can grow a memory, reallocating its `Vec`), and a
call-bearing body that also uses globals stays interpreted (its `gvals`
buffer is a snapshot the callee neither sees nor refreshes).
`lowerable` also gates `call_indirect` on 32-bit-addressed tables and
numeric callable types (the full trap-code table now round-trips every
interpreter trap for nested subtrees). Unit coverage drives an add/
wrapper/factorial-recursion module (every recursion step crosses the
compiled/interpreter boundary through the helper) and a `call_indirect`
module dispatching through a table element, including OOB element
indices. `wasmtest equiv` is green over `call.wast`, `call_indirect.wast`,
`fac.wast`, `func.wast`, `func_ptrs.wast`, `return_call.wast`,
`return_call_indirect.wast`, `switch.wast` (all 0 diverged) plus the
numeric/control and memory regressions. 34 compile tests, 70 total with
the feature, clippy clean in both configurations. Wave 2 remaining: the
table/`ref.func` value family whose refs must flow *through* compiled
code (`table.get/set`, `call_ref`, `ref.func` results) — that needs an
opaque ref representation on the compiled operand stack.

Function references flow through compiled code — the last Wave 2 piece.
The compiled value model now carries a (nullable or not) function
reference as an opaque u64 token (`values`: 0 is the null reference; a
function reference is tagged at bit 63 and packs its address's instance
and full function-index-space index), so `ref.func`, `ref.null func`,
`ref.is_null`, `table.get`/`table.set` over 32-bit funcref tables, and
`call_ref`/`return_call_ref` all lower. `clif_type` maps a funcref to
`I64` (a new `num_type` keeps the genuinely numeric gates — globals, the
call scratch — from accidentally accepting refs), params/locals/blocks
carry the tokens, and the store boundary marshals them (`run_compiled`
and the call helper encode/decode tokens; a ref param/result crosses to
an interpreted callee or back as its token). The runtime call helper
gained three modes: `call_ref` on a function token (null →
`NullFunctionReference`), and `table.get`/`table.set` with bounds
checking (the element is read/written as its token). The token model
needs no store-side registry — tokens encode the address directly, so a
compiled loop calling `ref.func` never allocates. Unit coverage drives a
funcref-parameter `apply` through `call_ref` (including a null
reference trap) and a module that `table.set`s `ref.func $add`, reads it
back with `table.get`, dispatches via `call_ref`, and observes the store
cross-call with `ref.is_null`. `wasmtest equiv` is green over `ref_func`,
`ref_is_null`, `call_ref`, `return_call_ref`, `table`, `table_get`,
`table_set`, `br_on_null`, `br_on_non_null` (0 diverged) plus the
call/call_indirect, numeric/control, and memory regressions. Externref/
exnref/GC heap refs, ref-typed globals, and `br_on_null`/`br_on_non_null`
lowering stay interpreted (bodies bail cleanly), so those suites remain
interpreter-on-both-sides. 36 compile tests, 72 total with the feature,
clippy clean in both configurations. Wave 2 is complete.

Wave 3's first slice landed: direct compiled-to-compiled re-entry. The
call helper now dispatches a resolved callee that is itself a compiled
module-defined function **natively** (`run_native_callee`), instead of
running every callee through an interpreter subtree. Each native level
allocates fresh caller-owned buffers for the callee's own memories
(descriptors from its instance's cells), its used globals (a `gvals`
buffer seeded from — and flushed back to — its cells, surviving traps),
and its internal-call scratch; arguments already spilled in the caller's
scratch double as the result buffer (`CompiledFunc::call_raw` invokes the
entry with raw pointers). After the callee returns, its global writes are
flushed to the store and the caller's memory descriptors are refreshed in
case a shared memory grew. A `native_depth` budget (`NATIVE_CALL_DEPTH`,
64 — sized for a 1 MB host main-thread stack even in debug builds)
bounds the native frames: past it the callee runs through the
interpreter, whose own frame budget then applies, so runaway recursion
still ends in a clean `CallStackExhausted` instead of a native stack
overflow. `return_call*` tails still route through the interpreter (its
frame-replacing tail call keeps those bounded). Unit coverage adds a
1500-deep countdown recursion that crosses the native budget, mixing
native frames with an interpreted fallback subtree. `wasmtest equiv`
stays green over `call`, `call_indirect`, `fac`, `switch`, `return_call*`,
`linking` (cross-instance native calls), `imports`, `global`, `memory`,
`memory_grow`, and the ref/table suites (0 diverged). 37 compile tests,
73 total with the feature, clippy clean in both configurations. Wave 3
remaining: compiled bodies whose callee graph can reach a resumable
external host import stay interpreted (the plan's one-host-call-mechanism
rule), so the host-boundary arm is already satisfied by exclusion; a
future perf wave can tune `NATIVE_CALL_DEPTH` (or make it release-only).

Whole-corpus equivalence gate: `wasmtest equiv waspec/test/core` now
compares **254 suites — the full core corpus (top level plus
`bulk-memory`, `exceptions`, `gc`, `memory64`, `multi-memory`,
`relaxed-simd`, and `simd`)** — through the compiled path against the
interpreter-forced oracle with **0 diverged**. Bodies the compiler cannot
lower yet run interpreted on both sides and cannot diverge, so this
proves every lowered instruction reproduces the interpreter exactly
across the entire corpus at once — no residual divergence in any
combination of the numeric/control/memory/global/call/ref subset the
waves have enabled.

Typed function references and the null-branch ops landed (Wave 4's
reference-types slice). The value model's function-reference check is
now module-aware: a `(ref $t)`/`(ref null $t)` whose type index resolves
to a function type rides the compiled stack as the same u64 token as an
abstract funcref (`heap_is_func`/`val_is_func_ref`/`carrier_type`), so
type-indexed refs work in params, locals, block types, `ref.func`,
`call_ref`, and tables whose element type is a typed function ref.
`ref.as_non_null` (trap `NullReference` on the null token), `br_on_null`
(conditional branch carrying the payload below the popped reference, or
pushing it back), and `br_on_non_null` (branch carrying the reference on
top of the payload when non-null; the null is consumed) all lower over
the funcref token model. Unit coverage adds a typed-`(ref null $t)`
`apply` through `call_ref`, a `br_on_null` loop exiting on a null
parameter, and a `ref.as_non_null` keep/trap pair. `wasmtest equiv` is
green over `call_ref`, `return_call_ref`, `ref_func`, `br_on_null`,
`br_on_non_null`, `ref_as_non_null` (0 diverged) and the whole core
corpus stays at 254 suites / 0 diverged. 40 compile tests, 76 total with
the feature, clippy clean in both configurations.

Externrefs ride the compiled token model (the extern/GC half of Wave
4's reference types). The token space gains an extern region — bit 62
marks an external reference, bits 60-61 select its payload kind
(host/i31/struct/array), and 60 bits carry the payload — so `RefValue::
Extern` encodes into the same zero-allocation u64 token as function
references (no store-side registry needed: tokens encode the value
directly, so compiled externrefs never allocate). The value model's
carrier check (`heap_is_carried`) now admits the abstract `extern` heap
alongside function refs, unlocking externref params/locals/results,
`ref.null extern`, and `table.get`/`table.set` over externref tables
(the table element gate became `table_carried_ref`). Boundary
marshaling is generic already (any ref param/result round-trips
through `ref_to_token`/`token_to_ref`), and the call helper's slot
decoder accepts extern params too. Unit coverage stores an externref
parameter (crossing the boundary as a packed extern token) into an
externref table, reads it back, and null-tests it. `wasmtest equiv` is
green over `table_get`, `table_set`, `table`, `ref_func`, and `call_ref`
(0 diverged) and the whole core corpus stays at 254 suites / 0 diverged.
41 compile tests, 77 total with the feature, clippy clean in both
configurations. GC heap types (i31/struct/array/exn and their tables)
and the GC object instructions remain interpreted.

Bulk-memory instructions lower: `memory.copy` (mode 5), `memory.fill`
(mode 6), `memory.init` (mode 7), and `data.drop` (mode 8) run through
the runtime helper with their operands spilled to the caller-owned
scratch region (addresses/offsets/length as u64 slots, each sized by the
memory's index type). The helper mirrors the interpreter exactly:
same-cell copies move through a temporary (`Vec::copy_within`, so
overlapping regions behave like memmove), cross-cell copies snapshot
the source first, `memory.init` reads the instance's data segment (a
dropped segment is empty, so any later init of nonzero length traps
`OutOfBoundsMemoryAccess`), and `memory.fill` truncates the byte value.
All memory widths lower (`memory.copy`'s length is i64 only when both
memories are memory64, per the interpreter). Unit coverage drives a
passive data segment through init, an overlapping same-memory copy, a
fill, and a drop-then-init trap sequence. `wasmtest equiv` is green over
the `bulk-memory` dir (8 suites, 0 diverged) and the whole core corpus
stays at 254 suites / 0 diverged. 42 compile tests, 78 total with the
feature, clippy clean in both configurations.

The table bulk ops lower (the rest of the interpreter's bulk-memory wave
surface): `table.size` (mode 9), `table.grow` (10), `table.fill` (11),
`table.init` (12), `table.copy` (13), and `elem.drop` (14) run through
the same runtime helper, with reference values riding scratch slots as
the same tokens `table.get`/`set` already use (null decodes back to a
null element, so fills/grows with nulls work). The helper mirrors the
interpreter exactly: same-cell copies move through a temporary
(`Vec::copy_within`), cross-cell copies snapshot the source first,
`table.init` reads the instance's element segment (a dropped segment is
empty, so any later nonzero-length init traps
`OutOfBoundsTableAccess`), and growth caps follow the table's address
type (`table.grow` returns the old length, or all-ones read at the
table's width for a failed grow). Both address models lower: a table64's
dst/len operands and `table.size`/`grow` results are i64, `table.copy`'s
length is i64 only when both tables are table64, and `table.init`'s
element offset and length stay i32 — exactly the interpreter's width
model. The gate admits any table whose element type the compiled value
model carries (funcref/externref/typed function ref) at either width.
Unit coverage drives a passive element segment through init, overlapping
same- and cross-table copies, a fill, grow/size incl. the 32-bit cap and
the table64 overflow failure, an out-of-bounds fill/init pair, and a
drop-then-init trap sequence, over both 32- and 64-bit tables. `wasmtest
equiv` is green over the whole core corpus: 254 suites, 0 diverged. 45
compile tests, 81 total with the feature, clippy clean in both
configurations.

Escaping exceptions lower: a compiled `throw` (runtime helper mode 15)
spills the tag's payload to the caller-owned scratch in parameter order
and runs a new throw helper that decodes it back into interpreter values
and parks an in-flight exception (`ExecFail::Exception`) as the store's
pending error, exactly like the interpreter's `throw`. The gate admits a
tag whose payload types all ride the compiled value model, so directly-
invoked exports that throw without an enclosing `try_table` now run
compiled; bodies that could catch (`try_table`) still stay interpreted,
and `throw_ref`/exnref payloads remain interpreted. The lowerer sets the
path dead after the helper (with a trap-return terminator on the
unreachable fall-through block, since a throw never returns success).
Unit coverage drives throws with empty, two-i32, f32, i64, and f64
payloads through both paths and asserts the stored exception payloads
match bit-for-bit (parameter order and types), plus a throw inside an
if-then-only body with a dead then-branch. `wasmtest equiv` is green
over the whole core corpus: 254 suites, 0 diverged. 46 compile tests, 82
total with the feature, clippy clean in both configurations.

Internal `i31` references ride the compiled token model (the first
`any`-hierarchy heap the compiler carries, following the func/extern
regions): token bit 60 (bits 61-63 clear) packs a canonicalized 31-bit
value, so `ref.i31` (i32 modulo 2^31), `i31.get_u` (zero-extend), and
`i31.get_s` (sign-extend bit 30, computed as `(v << 1) >> 1` exactly
like the interpreter) lower to inline Cranelift shifts/masks over the
token, a null token traps `NullI31Reference`, and `ref.eq` compares
tokens (identical to `RefValue` equality for the carried subset). The
carrier gate now admits the `I31` heap, which unlocks `(ref null i31)`
params/locals/results/blocks across the compiled boundary and i31
`table.get/set/grow/fill/init/copy` element tokens through the existing
table helpers; the call-slot decoder decodes any carried-ref token
(rather than only the concrete func/extern heaps). Unit coverage drives
wrap/unwrap round-trips over edge bit patterns, both null traps,
`ref.eq` over canonicalized pairs, an i31 parameter crossing the
boundary and back, and an i31ref table through init/grow/get/overlapping
copy reading stored values back. `wasmtest equiv` is green over the
whole core corpus: 254 suites, 0 diverged. 48 compile tests, 84 total
with the feature, clippy clean in both configurations.
