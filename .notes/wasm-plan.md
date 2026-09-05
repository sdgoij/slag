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
decodes end to end). `wasmtest run` now converts suites with `wast2json-rs`
(fallback `wast2json`) and executes commands, classifying outcomes by what
the current engine can judge (decode/`assert_malformed` now;
`assert_invalid` pending validation in Cut 2; invocations pending execution
in Cut 3) — `utf8-invalid-encoding.wast` is 176/176 pass. Tool caveat:
`wast2json-rs` 0.1.0 cannot parse inline-`binary` modules inside
`assert_malformed` (binary.wast, utf8-custom-section-id.wast) and can
silently truncate some suites (type.wast), so the full Cut-1 gate needs a
more complete converter (wabt) or a runner-side fallback for those files.

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

- **`wast2json`** (wabt, a *test/dev* tool — never a runtime dependency)
  converts each `.wast` into one `.wasm` per module plus a `.json` of
  commands (`module`, `assert_return`, `assert_trap`, `assert_invalid`,
  `assert_malformed`, `assert_unlinkable`, `assert_exhaustion`,
  `register`, `action`). Feature suites are converted with the matching
  `--enable-*` flags.
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
- DoD: the `test/js-api` suite (61 files) via a WPT-lite harness that
  loads `wasm-module-builder.js` and the `.any.js` tests against the Slag
  embed (reusing the existing wasm demo host), and — once green — vendoring
  upstream test262's `built-ins/WebAssembly` fixtures into the pinned
  corpus so the existing sweep covers the JS API end to end.

## 6. Verification workflow

- Per-cut: `cargo test -p wasm` (unit tests for decode/validation/exec
  edges, floats, traps) plus `cargo run -p wasmtest` over the cut's gate
  files; the runner reports pass/fail/hang per file like the test262 sweep.
- Full-workspace gates after every cut: `cargo clippy -- -D warnings` and
  the native/wasm builds of the embedding demo (the browser demo gets a
  `WebAssembly` smoke once Cut 10 lands).
- A cut is only "done" when its suite reports **0 failures** and any
  deliberate exclusions have a written taxonomy entry (the test262
  convention), not silent skips.

## 7. Future work (post-baseline, deliberately out of the cuts above)

- **Compilation**: a wasm-to-native pass (the obvious long-term path is
  Cranelift, reusing the JIT dependency and its `Step`-lowering lessons);
  until then the interpreter is the sole execution path.
- **Threads/shared memory**: `shared` memories + `atomic.*` ops wired into
  the existing workers/`Atomics` machinery once multi-agent memory is in
  place.
- **Web API** (`WebAssembly.instantiateStreaming`/fetch integration,
  `document/web-api`) — a host concern for the browser demo, not the core
  engine.

## 8. Decisions (ratified)

1. **`wast2json` is the conformance compiler** (wabt, dev/test-only — never
   a runtime dependency) for now; revisiting the OCaml reference interpreter
   is deferred until the runner needs it as an oracle.
2. **NaN policy: canonical quiet NaN** for arithmetic results (V8-style),
   payloads only where an operation's semantics require propagating one.
3. **Feature ordering after Cut 5**: exceptions → SIMD/relaxed-SIMD →
   memory64/multi-memory → GC; the JS API (Cut 10) may start in parallel
   once Cut 5 lands.
4. **Naming/wiring**: `crates/wasm` (core) + `crates/wasmtest` (harness);
   `runtime` gains a `wasm` feature with `Context::install_wasm()`, enabled
   by default in the CLI alongside `fs`.
