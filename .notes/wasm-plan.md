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

## 9. Status

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
  `wast2json` on PATH, then this machine's wabt build, then the Rust
  `wast2json-rs`), invoked with `--enable-function-references --enable-gc`.
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
  beyond wabt 1.0.41 (and `wast2json-rs`).

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
