# Slag

> The stony waste matter separated from metals during the smelting or refining of ore.

> A test262 runner. It also happens to execute JavaScript.

Slag is a from-scratch, spec-faithful JavaScript engine in Rust, implementing the ECMAScript® 2026 Language Specification (17th edition). 

The full pinned `test262` corpus is the regression net: **51,669 pass / 0 fail / 0 crash / 0 hang** across 
the `language`, `built-ins`, Annex B and `intl402` sweep areas. That includes **proper tail calls** the 
**Intl**  surface (ECMA-402 Cuts 1–8: NumberFormat, Locale, PluralRules, RelativeTimeFormat, ListFormat, 
DisplayNames, DateTimeFormat, Collator, Segmenter, DurationFormat), and **Temporal**. 

It ships a command-line runner/REPL, a full WebAssembly engine  and its JavaScript API, a small 
embedding API, and drop-in JavaScriptCore C-API bindings.

## Highlights

- **Spec-faithful** — written chapter-by-chapter against the vendored
  `spec.html`; abstract operations keep the spec's names, ordering, and
  edge cases so conformance bugs are easy to diff.
- **Conformant** — 51,669 passing fixtures, **0 failures / 0 crashes /
  0 hangs** across 51,979 `test262` fixtures (runnable-only; see
  `.notes/conformance.md`). Proper tail calls: 34/34 `tco-*`. Workspace
  tests: 4,887 pass / 0 fail.
- **Complete modern feature surface** — modules (source-text module
  machinery, top-level await, dynamic import), async/await, generators,
  Proxy/Reflect, TypedArrays, SharedArrayBuffer/Atomics with worker
  threads, full Intl (ECMA-402 Cuts 1–8), and Temporal (the intl402×
  Temporal integration, Cut 9, is in flight).
- **Full WebAssembly runtime + JS API** — a from-scratch engine
  (`crates/wasm`) plus the document/js-api surface, validated against the
  pinned `waspec` corpus: **64,594 core checks / 0 fail** and **1,001
  JS-API tests / 0 fail**. Covers GC (struct/array/i31, casts),
  exceptions, SIMD + relaxed SIMD, memory64, multi-memory, and bulk
  memory; every realm gets the `WebAssembly` global by default — a
  default-on cargo feature (V8/Node parity — no bare-`.wasm`-file
  mode).
- **Compiled WebAssembly** — the wasm engine's Cranelift backend
  (`crates/wasm/src/compile.rs`, "Cut 11") lowers function bodies to native
  machine code and is **on by default for native targets**. It compiles
  lazily (a body compiles the first time execution reaches it, so a module
  pays only for what runs) and falls back to the interpreter per body, which
  stays the equivalence oracle; the compiled path reproduces the corpus's
  totals exactly. Native-only by construction: a wasm32 embed — the browser
  demo included — always runs the interpreter.
- **Cranelift JIT** — compiled bodies run as native machine
  code via [Cranelift](https://cranelift.dev): inline number/string fast
  paths, direct-mapped global/member value cells, and register-resident
  fast loops. Compiled with the `jit` feature (on by default in the CLI;
  the embed crate opts in with `--features slag/jit`) and active for every
  run unless `--jitless` disables it; `--jit-bench` times JIT vs
  interpreter. Design, status, and remaining work: `.notes/jit-report.md`.
- **Portable** — no third-party runtime dependencies beyond the Rust
  standard library (see `PLAN.md` §4.10), with opt-in exceptions: the
  experimental JIT (`crates/jit`) pulls in Cranelift and `region` when the
  `jit` feature is enabled (on by default in the CLI; the embed crate opts
  in with `--features slag/jit`), the `raylib` feature
  (`crates/runtime/src/raylib.rs`) compiles raylib's C library for the `rl`
  host module, and the `jsc` drop-in is a separate on-request build
  (below). The Unicode property
  tables are generated at compile time from the pinned corpus fixtures,
  so they can never drift from what the tests assert.
- **Embeddable** — the `slag` crate exposes the Rust embedding API
  (`Context`, `JsValue`/`JsObject`, `HostCallbacks`, optional Cranelift JIT
  hook), plus a drop-in JavaScriptCore C API (`crates/jsc`) built on
  request (`cargo build -p cli --features jsc`, or `cargo build -p jsc`).

## Quick start

Run it in the browser: the [live demo](https://sdgoij.github.io/slag/)
compiles the same engine to WebAssembly.

Requires a stable Rust toolchain (edition 2024) and the pinned `test262`
submodule — the `unicode` build script derives the RegExp property-escape
tables from the corpus fixtures at compile time and fails with
instructions if the submodule is missing.

```sh
git submodule update --init   # the pinned test262 corpus
cargo build --release
target/release/slag --version
target/release/slag script.js [args...]   # run a script
target/release/slag                       # REPL
```

The CLI exposes `process.argv` and a minimal `fs` (`readFileSync`/
`readdirSync`/`statSync`) to scripts — `readFileSync` returns the raw bytes as a
`Uint8Array` unless a `'utf8'` encoding is passed — and accepts `--dump-ast`,
`--dump-tokens`, `--print-bytecode` (dump the compiled `Step` stream),
`--bench` (interpreter micro-benchmarks), and — when the JIT feature is
compiled, the default — `--jitless` (disable the Cranelift JIT for the run;
it is on by default) and `--jit-bench` (time JIT vs interpreter). Scripts
get the full `WebAssembly` global in every realm, matching V8/Node; the
JS API is a default-on cargo feature (drop it with
`cargo build -p cli --no-default-features --features jit`). `--help`
lists the optional flags and the compiled features (`wasm`, `jit`, ...).
`--jsx` parses
the input with the opt-in JSX extension (`<element/>` syntax desugaring to
`rlx.h(...)` calls). Building the
CLI with the `raylib` feature exposes the `rl` host module to every
script (`cargo run -p cli --features raylib -- game.js`); add `raygui`
(`--features raylib,raygui`) to also get raygui's controls as `rl.gui*`. The
`--stack-size`, `--max-old-space`, and `--harmony-*` knobs are accepted
for compatibility (no-ops for now).

CI builds and publishes six archives on a `v*` tag —
`slag-<version>-<platform>` and `slag-raylib-<version>-<platform>` for
windows-x86_64, linux-x86_64 and linux-aarch64, as `.zip` on Windows and
`.tar.gz` elsewhere, alongside a `SHA256SUMS` file. Each archive is the
single `slag` binary (the raylib variant links raylib and raygui statically)
plus this readme and the licence.

## Embedding

The `slag` crate is the Rust embedding API — a single dependency for
`Context` (a fresh agent, realm, and host globals per instance), the
`JsValue`/`JsObject` handle types, `HostCallbacks`, and (with the `jit`
feature) the Cranelift JIT hook. A full walkthrough lives in
`crates/slag/examples/embed.rs` (`cargo run -p slag --example embed`; add
`--features slag/jit` for the JIT).

```rust
use slag::{Context, JsValue};

let mut context = Context::new()?;

// Evaluate a script in the global scope.
let value = context.eval("1 + 2")?;
println!("{value}"); // 3

// Call a script-defined function with host-provided arguments.
let function = context.eval("function double(x) { return x * 2; }; double")?;
let doubled = context.call(
    &function,
    &JsValue::undefined(),
    &[JsValue::number(21.0)],
)?;
println!("{doubled}"); // 42
```

`HostCallbacks` routes `console` output and promise-rejection tracking;
`install_process_argv` installs a Node-style `process.argv`. Native functions
register from Rust with `Context::register_fn` (a global) or
`Context::create_function` plus `Context::create_object` (a namespace object to
hang them on); the callback receives a `FunctionCall` — `this`, the arguments,
agent-backed coercions, re-entrant `call`/`construct`/`eval`, and value
construction — and returns `Ok(JsValue)` or `Err(JsError)`, with
`JsValue::thrown()` for throwing an arbitrary value. `Context::create_constructor`
builds a host constructor instead
(the fresh instance arrives as `this`, and `FunctionCall::{is_construct,
new_target}` distinguish a `new` call), and `Context::define_accessor` defines a
getter/setter pair backed by host closures:

```rust
context.register_fn("host_sum", 2, Box::new(|call| {
    let a = call.arg(0).and_then(|v| v.as_number()).unwrap_or(0.0);
    let b = call.arg(1).and_then(|v| v.as_number()).unwrap_or(0.0);
    Ok(JsValue::number(a + b))
}))?;
```

For declarative
UI, `Context::install_rlx` installs a small virtual-element layer — `rlx.h`
trees driven frame-by-frame with `rlx.present`, retained per-path state via
`rlx.useState`, and control events dispatched to `onClick`/`onChange` —
that can sit on top of the raylib surface below. `Context::eval_jsx` parses
scripts with the opt-in JSX extension, which desugars `<element/>` syntax to
`rlx.h(...)` calls.

### The `rl` host module

With the `raylib` feature, `Context::install_raylib` exposes a host
surface to scripts as the `rl` global: window control, `beginDrawing`/
`draw*`/`endDrawing` 2D primitives, a small 3D surface
(`beginMode3D`/`drawCube`/`drawGrid`, needs raylib's `rmodels` module),
input queries, plus raylib's palette and key/mouse
constants. Building with `raygui` as well (`--features raylib,raygui`)
additionally installs raygui's immediate-mode controls (`rl.guiButton`,
`rl.guiSlider`, ...) for UI inside the render loop. A script owns the whole render loop, exactly like a raylib C
example — `while (!rl.windowShouldClose()) { rl.beginDrawing(); ...;
rl.endDrawing(); }`. raylib's window state is bound to the thread that
installed the module; calls from worker agents throw a clean `TypeError`
instead of racing it.

```sh
cargo run -p slag --example raylib_demo --features slag/raylib
cargo run -p slag --example raylib_voxel --features slag/raylib,slag/jit   # first-person voxel sandbox
cargo run -p slag --example raygui_demo --features slag/raygui            # raygui control panel (also needs a display)
cargo run -p slag --example rlx_demo --features slag/raygui              # declarative JSX UI (rlx layer, state + events)
cargo run -p cli --features raylib -- game.js
```

## Conformance

The pinned `test262` submodule is the regression net: the sweep runner
(`cargo run --release -p test262 --bin sweep`) runs any area in parallel,
timeout-guarded batches, and the `unicode` build script derives the
property-escape tables from the same fixtures. CI sweeps all four areas on
Linux and Windows and fails on a `fail`, `crash` **or** `hang` — at a 30s
batch deadline with a 60s per-fixture recheck rather than the 15s local
methodology, since the runner VMs are slower than the machine below and the
deadline is wall clock (a batch that overruns is re-decided fixture by
fixture, so a reported `hang` is one that missed the recheck deadline on its
own); current result (release build, this machine, 15s deadlines):

| Area | Total | Pass | Fail | Skip | Hang | Pass % of runnable |
|---|---|---|---|---|---|---|
| language | 23,724 | 23,721 | 0 | 3 | 0 | 100.0% |
| built-ins | 23,812 | 23,657 | 0 | 155 | 0 | 100.0% |
| annexB | 1,086 | 1,086 | 0 | 0 | 0 | 100.0% |
| intl402 | 3,357 | 3,205 | 0 | 152 | 0 | 100.0% |
| **Total** | **51,979** | **51,669** | **0** | **310** | **0** | **100.0%** |

The skips are the out-of-scope `await-dictionary` (89) and `ShadowRealm`
(64) proposal fixtures, 152 `intl402` fixtures tagged with `Intl.*` features
whose plan cuts have not landed (`.notes/intl-plan.md`), one stale Temporal
fixture, and 4 fixtures this Windows checkout cannot run: the submodule is
checked out CRLF by `core.autocrlf`, so their byte-exact assertions read
`\r\n` where the corpus asserts `\n` (the skip is conditional — a clean LF
checkout runs them). The 458 hangs this table used to report are closed: the
RegExp property-escape cluster, the dense-elements and typed-array work, and
the Temporal `since`/`until` day-difference loop (one iteration per day over
edge-of-range dates, now a closed-form epoch-day difference) all landed
since, and the last one — `intl402`'s quadratic walk over every pair of the
444 supported time zones — went from 18.7s to 4.4s when the intrinsics probe
stopped encoding a UTF-16 string per name lookup (see Performance). The full
methodology and triage live in `.notes/conformance.md`.

## WebAssembly

Slag also ships a from-scratch WebAssembly engine and the full
`WebAssembly` JavaScript API (document/js-api). The engine
(`crates/wasm`) implements binary decoding, validation, and execution for
the pinned WebAssembly spec corpus, covering the merged post-MVP
proposals: typed references and tail calls, tables and bulk memory,
exceptions (`throw`/`try_table`/`throw_ref`), SIMD and relaxed SIMD, GC
(`struct`/`array`/`i31`, `ref.test`/`ref.cast` and subtyping), memory64,
and multi-memory. The JS-API layer surfaces
`WebAssembly.Module/Instance/Memory/Table/Global/Tag/Exception`, the error
constructors, `compile`/`instantiate`/`validate`, `WebAssembly.JSTag`,
BigInt/i64 across the JS boundary, and shared-memory grow semantics.
`WebAssembly` is installed in every realm by default, so CLI/embed
scripts use it exactly as they would under V8 or Node (there is
deliberately no bare `.wasm`-file mode — Node and d8 do not have one
either). The JS API is a default-on cargo feature of `runtime` (the CLI
mirrors it): `cargo build -p cli --no-default-features --features jit`
produces a wasm-free binary.

Conformance is gated by the pinned `waspec` submodule (init it with `git
submodule update --init` alongside `test262`): the core corpus — the
baseline files plus each proposal directory — and the JS-API fixtures all
report **0 failures and 0 pendings**:

| Suite | Pass / fail |
|---|---|
| baseline `core/*.wast` | 20,662 / 0 |
| `exceptions/` | 105 / 0 |
| `simd/` | 25,990 / 0 |
| `relaxed-simd/` | 77 / 0 |
| `multi-memory/` | 912 / 0 |
| `memory64/` | 8,709 / 0 |
| `gc/` | 654 / 0 |
| `bulk-memory/` | 7,485 / 0 |
| **core total** | **64,594 / 0** |
| JS-API (`js-api`) | 1,001 / 0 |

The only non-runnable files are documented taxonomy entries, not silent
skips: three harness-tooling files in the baseline (`annotations`,
module-linking `instance`, and `names` — confusing-unicode export names),
one `gc/type-subtyping.wast` whose multi-supertype text the `wast`
grammar rejects, and the out-of-scope `gc`/`js-string` JS-API proposals
(the embedder-limit `limits.any.js` runs on demand). The rules that last
file covers are pinned instead by
`crates/wasmtest/fixtures/type-subtyping.wast`, which
`cargo test -p wasmtest` gates.

Run the sweeps with the `wasmtest` runner. Duplicate `.wast` file names
across directories share the runner's cache key, so each suite directory
runs separately:

```sh
cargo run -p wasmtest -- run --strict waspec/test/core/*.wast  # baseline (top-level files)
cargo run -p wasmtest -- run --strict waspec/test/core/simd     # ... and each proposal dir
cargo run -p wasmtest -- run --strict waspec/test/core/gc
cargo run -p wasmtest -- jsapi waspec/test/js-api               # the JS-API fixtures
```

`wasmtest run` exits non-zero on a `fail`; `--strict` makes it exit non-zero on
a `pending` as well, which is how the "0 pendings" claim above is enforced
rather than trusted — the documented sweeps pass `--strict`. A `skip` is a
written taxonomy entry in `crates/wasmtest/wasm-exclusions.txt`, never a silent
miss, and never fails. The decoder's malformed/unsupported boundary — the
encodings the vendored corpus never reaches — is *additionally* pinned by
`crates/wasmtest/fixtures/decoder-classification.wast`, which
`cargo test -p wasmtest` runs and fails on any pending.

`--compiled` runs a sweep through the compiled path instead (it is the native
default, so plain `run` forces the interpreter to keep the oracle reachable);
`wasmtest equiv` runs each suite through both and reports the first command
whose outcomes diverge.

The implementation plan, cut history, and status live in
`.notes/wasm-plan.md`.

## Performance

Two harnesses measure the engine, and both are reproducible from the repo:
the CLI's micro-suite (`slag --jit-bench` runs the same bodies through the
Cranelift JIT and the interpreter) and the cross-engine workload corpus
(`node tools/corpus/bench.js` runs 37 workloads under Slag and V8, each with
its JIT and with it disabled — see `tools/corpus/README.md`). The numbers
below are one machine (AMD Ryzen 9 7950X, Windows 11, rustc 1.96.0, node
v24.12.0): medians of three runs for the micro-suite, one run for the corpus.
The standing detail, the benchmark gates, and the deferred work live in
`.notes/perf.md`.

### The JIT against the interpreter

Ratio < 1 means the JIT is faster than the interpreter (`result-ok` checks
both paths computed the same value):

| Body | Interpreter | JIT | Ratio |
|---|---|---|---|
| `arithmetic` | 8.15 ms | 0.627 ms | 0.08x |
| `bare loop` | 7.74 ms | 0.621 ms | 0.08x |
| `wide leaf call` | 21.93 ms | 1.679 ms | 0.08x |
| `property read` | 9.77 ms | 0.797 ms | 0.08x |
| `function calls` | 6.18 ms | 0.757 ms | 0.12x |
| `global read` | 11.15 ms | 1.384 ms | 0.12x |
| `buildString shape` | 93.28 ms | 13.840 ms | 0.15x |
| `string concat` | 1.28 ms | 0.191 ms | 0.15x |
| `typed-array length` | 12.12 ms | 1.941 ms | 0.16x |
| `buildString full` | 73.36 ms | 16.631 ms | 0.23x |
| `apply leaf call` | 20.80 ms | 7.594 ms | 0.37x |
| `typed-array write` | 31.15 ms | 12.565 ms | 0.40x |
| `compound assign` | 3.61 ms | 1.562 ms | 0.43x |
| `builtin call` | 5.56 ms | 2.445 ms | 0.44x |
| `non-leaf call` | 18.89 ms | 13.045 ms | 0.69x |

`builtin call` and `non-leaf call` are the two rows the pooled-`Vm` and
builtin-verdict work added; every other call row's callee is a certified leaf,
which the JIT inlines. Neither of these can inline — a body that contains a
call is not a leaf, and a crux-native builtin is not a JS leaf — so both engines
run the general call path there and the ratio is set by that machinery, not by
code generation. `.notes/perf.md` has the per-shape probe and what each row
moved.

### Against V8

The corpus runner checks parity as well as time: all four engine/mode
combinations return the same value for every workload (`mismatches 0`).
Gap = Slag ms / V8 ms, so > 1 means V8 was faster:

| Family | Workloads | JIT gap | Interpreter gap |
|---|---|---|---|
| arrays | 6 | 42.13x | 7.50x |
| builtins | 5 | 18.82x | 7.12x |
| calls | 6 | 28.20x | 6.05x |
| control | 5 | 70.16x | 7.03x |
| language | 3 | 15.98x | 8.81x |
| objects | 7 | 43.74x | 3.34x |
| strings | 5 | 27.54x | 6.59x |
| **All** | **37** | **36.72x** | **6.35x** |

A sample of the per-workload rows (the command above prints all 37), ms per
`bench()` call:

| Workload | Slag JIT | Slag interp | V8 JIT | V8 `--jitless` | JIT gap | Interp gap |
|---|---|---|---|---|---|---|
| `arrays/for_of_dense.js` | 40.1 | 64.7 | 1.8 | 63.9 | 22.79x | 1.01x |
| `arrays/typed_array.js` | 103.8 | 210.5 | 1.1 | 39.7 | 92.22x | 5.30x |
| `builtins/json_roundtrip.js` | 88.4 | 87.0 | 13.1 | 16.0 | 6.75x | 5.42x |
| `builtins/math_intrinsics.js` | 100.3 | 127.8 | 149.7 | 185.9 | 0.67x | 0.69x |
| `calls/direct_leaf.js` | 10.5 | 74.2 | 1.1 | 37.5 | 9.10x | 1.98x |
| `calls/recursive_fib.js` | 415.5 | 557.0 | 7.4 | 33.5 | 56.00x | 16.61x |
| `control/generator_loop.js` | 94.1 | 115.9 | 2.4 | 9.6 | 38.96x | 12.10x |
| `objects/destructure.js` | 166.6 | 243.8 | 0.8 | 53.4 | 209.20x | 4.57x |
| `objects/own_read.js` | 2.9 | 43.1 | 1.3 | 49.7 | 2.31x | 0.87x |
| `objects/warm_store.js` | 49.8 | 133.8 | 2.3 | 54.6 | 21.23x | 2.45x |
| `strings/char_ops.js` | 28.2 | 34.6 | 0.4 | 6.3 | 75.39x | 5.46x |

Read the gaps as "where the work is", not as a verdict on the engine shape:
the corpus's own README records the workloads V8 folds or scalar-evolves to a
near-constant (which is why `destructure` and the other such rows have
outsized JIT ratios), and a sub-1 gap means Slag was faster on that workload.

### Temporal and Intl dispatch (fixed)

A Temporal property read or method call used to cost 11-41µs against ~0.25µs
for a `Date` method. Three changes, all keyed off
`crates/runtime/src/realm.rs`:

1. The intrinsics table was keyed by `JsString` — which is UTF-16, so every
   `Intrinsics::get(name)` probe encoded a fresh UTF-16 string before it could
   hash. Keyed by `Rc<str>`, a probe borrows instead: a `PlainDate` getter
   11.2µs → 2.0µs, `withTimeZone` 41.6µs → 7.5µs.
2. `define` now also records each intrinsic's name(s) by function id (a set per
   id, because spec aliases such as `%Array.prototype.values%` /
   `%Array.prototype[Symbol.iterator]%` are one object), so a chain dispatcher
   resolves its callee once per call and its arms become string compares
   (`ResolvedNames::is`) instead of table probes: 155 probe sites across the
   temporal (`shell`, `duration`, `instant`, `mod`) and Intl (`mod` + 11
   modules) chains, plus 13 construct-side sites.
3. Both namespace fronts (`builtins/intl/mod.rs`, `builtins/temporal/mod.rs`)
   then walked their sub-dispatchers in turn, each of which resolved the
   callee's names again — thirteen times over for one `format` call. They now
   route on the `%Intl.<Name>…%` / `%Temporal.<Name>…%` prefix first
   (`component_of` plus a table of sub-dispatcher pointers) and keep the walk
   as the fallback for the names no component claims (the
   `%IntlSegmentsPrototype…%` iterators), so a routing miss costs time and
   never a verdict.

Final: getters **0.63µs** (`PlainDate.prototype.year`; `day`, `hour`,
`epochNanoseconds` and `timeZoneId` land at 0.57-0.92µs), `withTimeZone`
**2.3µs**, `Intl.DateTimeFormat.prototype.format` 5.8µs → 3.1µs. The control
row does not move (`Date.getTime` 0.26µs), and `--jitless` tracks the same
rows (0.62-0.99µs, `withTimeZone` 2.5µs), so this is the dispatch and not code
generation.
`Intl.NumberFormat` construction (~9.5µs) is locale resolution rather than
dispatch, which is why the routing leaves it alone. The first two steps are
also what cut `control/generator_loop` from 182 ms to 94 ms in the corpus above
and closed `intl402`'s zone-matrix fixture (18.7s → 4.4s).

What is left is the methods' own work — a getter still costs ~0.6µs against
the 11.2µs it started at — plus the twelve other chains (`array_buffer`,
`iterator`, `generator`, `promise`, `error`, `symbol`, `function`, `weakref`,
`disposable`, `proxy`, `reflect`, `module_source`), which keep the same probe
shape and can take the same mechanical conversion (a `let resolved = …` per
dispatcher plus the condition rewrite); their measured costs are 0.4-3µs per
call, so the win there is smaller. Twelve modules already dispatch O(1) through
the `handler_for`/`BuiltinHandler` tables registered at `Intrinsics::define`
time (`crates/runtime/src/builtins/array.rs` is the model) and need nothing.

## Repository layout

| Crate | Responsibility |
|---|---|
| `unicode` | Code-point tables, case conversion, ID_Start/ID_Continue, derived `\p{...}` tables (generated from the corpus at build time) |
| `byteblock` | The refcounted byte block behind an ArrayBuffer's `[[ArrayBufferData]]` (an `Rc<RefCell<Vec<u8>>>`; atomic words under `workers`), with the geometry box the JIT reads. A leaf crate so the wasm engine can share one block with the runtime — the prerequisite for aliasing a linear memory instead of copying it |
| `crux` | `Value`, strings, property keys, completion records, GC handles |
| `syntax` | `SourceText`, `Span`, `Token`, the full AST |
| `lexer` | Tokenizer: lexical goals, comments, literals, ASI |
| `parser` | Recursive-descent parser, cover grammar, early errors |
| `regexp` | RegExp pattern parser + backtracking matcher |
| `runtime` | Realms, environments, evaluation, modules, all built-ins (incl. Intl + Temporal), `Context` |
| `wasm` | WebAssembly engine: decode, validation, and execution (GC, exceptions, SIMD + relaxed SIMD, memory64/multi-memory, bulk memory) |
| `wasmtest` | The pinned `waspec` corpus + the wasm conformance runner (`run` for core, `jsapi` for the JS-API fixtures) |
| `jit` | Experimental Cranelift JIT backend for the interpreter's certified `Step` bytecode (CLI feature `jit`, on by default) |
| `ffi` | Shared C-ABI plumbing for the drop-in surfaces (handle tables, value/string marshaling) |
| `jsc` | Drop-in JavaScriptCore C API (`JSContextRef` family) backed by Slag; not in the default build (`cargo build -p jsc`, or alongside the CLI with `cargo build -p cli --features jsc`) |
| `cli` | The `slag` binary (script runner + REPL) |
| `test262` | The pinned corpus + the sweep runner |

## Documentation

- `PLAN.md` — the implementation plan, per-phase spec coverage, and status
- `.notes/conformance.md` — conformance methodology, results, and triage
- `.notes/gc-plan.md` — the GC milestone plan (arena heap + mark-sweep, cut-by-cut)
- `.notes/intl-plan.md` — the ECMA-402 (Intl) implementation plan and cut status
- `.notes/jit-report.md` — the experimental Cranelift JIT: design, fast paths, hardening, validation, and remaining work
- `.notes/memory-model.md` — ECMAScript ch. 28 shared-memory model
- `.notes/wasm-plan.md` — the WebAssembly engine plan: cuts, decisions, conformance status
- `.notes/perf.md` — performance milestones and deferred work

## License

MIT OR Apache-2.0
