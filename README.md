# Slag

> The stony waste matter separated from metals during the smelting or refining of ore.

> A test262 runner. It also happens to execute JavaScript.

Slag is a from-scratch, spec-faithful JavaScript engine in Rust, implementing the ECMAScript® 2026 Language Specification (17th edition). 

The full pinned `test262` corpus is the regression net: **51,997 pass / 0 fail / 0 crash / 0 hang** across 
the `language`, `built-ins`, Annex B and `intl402` sweep areas. That includes **proper tail calls** the 
**Intl**  surface (ECMA-402 Cuts 1–8: NumberFormat, Locale, PluralRules, RelativeTimeFormat, ListFormat, 
DisplayNames, DateTimeFormat, Collator, Segmenter, DurationFormat), and **Temporal**. 

It ships a command-line runner/REPL, a full WebAssembly engine  and its JavaScript API, a small 
embedding API, and drop-in JavaScriptCore C-API bindings.

## Highlights

- **Spec-faithful** — written chapter-by-chapter against the vendored
  `spec.html`; abstract operations keep the spec's names, ordering, and
  edge cases so conformance bugs are easy to diff.
- **Conformant** — 51,997 passing fixtures, **0 failures / 0 crashes /
  0 hangs** across 51,998 `test262` fixtures (runnable-only; see
  `.notes/conformance.md`). Proper tail calls: 34/34 `tco-*`. Workspace
  tests: 5,633 pass / 0 fail.
- **Complete modern feature surface** — modules (source-text module
  machinery, top-level await, dynamic import), async/await, generators,
  Proxy/Reflect, TypedArrays, SharedArrayBuffer/Atomics with worker
  threads, full Intl (ECMA-402 Cuts 1–8) including its Temporal integration
  (Cut 9), and Temporal.
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
constants. Textures and images are part of it as well: `loadTexture` and
`loadImage` resolve an embedded asset by name (any format raylib decodes,
from the name's extension) or a path on disk, `textureWidth`/`textureHeight`
and `imageWidth`/`imageHeight`/`imagePixel` read what they hold, and
`drawBillboardRec`/`drawQuad3D` draw a source rectangle — a flipbook frame,
or a decal lying on the ground — with `beginBlendMode`/`endBlendMode`
(raylib's `BLEND_*` modes, the custom pair included: `setBlendFactors` and
`setBlendFactorsSeparate` take the `BLEND_FACTOR_*`/`BLEND_EQUATION_*` enums),
`setTextureFilter` and `unloadTexture` alongside.
The binding table in `crates/runtime/src/raylib.rs` is the authoritative
surface list. Building with `raygui` as well (`--features raylib,raygui`)
additionally installs raygui's immediate-mode controls (`rl.guiButton`,
`rl.guiSlider`, ...) for UI inside the render loop. `--features gpu-skinning`
moves skeletal animation off the CPU: raylib then uploads each mesh's bone
attributes and leaves the per-vertex deform buffers unallocated, so
`updateModelAnimation` only computes the bone matrices and the model's own
shader does the skinning — route the model through one that declares
`boneMatrices`, `vertexBoneIndices` and `vertexBoneWeights` (`setModelShader`),
since raylib's default shader does not skin, and read `rl.GPU_SKINNING` to learn
which way a build went. A model that cannot go through such a shader — a mod's,
or one whose shader failed to compile — gets the CPU pass back for itself with
`rl.setModelCpuSkinning(model, true)`. A script owns the whole render loop, exactly like a raylib C
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

### The `v8` crate and Deno

`crates/v8` is a stand-in for `rusty_v8`: it is named `v8` at the API version it
replaces (`150.4.0`), so a host that depends on the crates.io `v8` resolves to
it. Where `rusty_v8` crosses into C++ through `binding.cc` and bindgen, this is a
pure-Rust implementation of the `v8::` surface on top of the engine — handles,
scopes, isolates, contexts, modules, promises, and the inspector's `Runtime`
domain, enough to drive `deno repl`. It is not in the default build, like
`crates/jsc`: `cargo build -p v8` builds it on request, and nothing in the
workspace depends on it. The engine has no interpreted mode, so the JIT is a
dependency of this crate rather than a feature of it.

The one consumer is Deno, and `tools/build-deno.py` is how it is wired up: it
clones a pinned Deno commit, redirects the crates.io `v8` — which Deno's
`deno_v8` facade pulls — to `crates/v8` with a `[patch.crates-io]` handed to
cargo through `--config`, so the checkout is never edited, reconciles the lock,
and builds `-p deno`:

```sh
python3 tools/build-deno.py            # release; also --debug, --pin, --deno-dir, --dry-run
./deno/target/release/deno run -A tools/deno_smoke.js
./deno/target/release/deno run -A tools/deno_surface.js
```

The two harnesses are the embedding checklist — `deno_smoke.js` (32 cases of
ECMAScript and web-platform surface) and `deno_surface.js` (29 cases of what a
host leans on: workers, `serve`/HTTP, WebSocket, `node:` builtins, streams, the
crypto and fs subtleties) — and both pass under the built `deno`. CI builds Deno
on Slag for linux-x86_64, linux-aarch64 and windows-x86_64 and attaches the
archives to a release. The Linux build additionally needs `cmake` (`libz-sys`
builds zlib-ng through it) and `libclang-dev` (bindgen for `libsqlite3-sys`);
the Windows build, on MSVC, needs neither.

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
| language | 23,726 | 23,726 | 0 | 0 | 0 | 100.0% |
| built-ins | 23,821 | 23,820 | 0 | 1 | 0 | 100.0% |
| annexB | 1,086 | 1,086 | 0 | 0 | 0 | 100.0% |
| intl402 | 3,365 | 3,365 | 0 | 0 | 0 | 100.0% |
| **Total** | **51,998** | **51,997** | **0** | **1** | **0** | **100.0%** |

The one remaining skip is a stale Temporal fixture (`+275760-09-12T00:00:01Z`
`relativeTo` is out of range per the current spec). Every other gate this
table used to carry has closed: `await-dictionary` (89) and `ShadowRealm` (64)
landed and their gates were removed, the 152 `intl402` proposal skips landed
with the `Intl.Locale-info` and canonical-time-zone work, and the 4
CRLF-affected fixtures run under a clean LF checkout. The 458 hangs this table
used to report are closed too: the RegExp property-escape cluster, the
dense-elements and typed-array work, and the Temporal `since`/`until`
day-difference loop all landed, and the last one — `intl402`'s quadratic walk
over every pair of the 444 supported time zones — went from 18.7s to 4.4s when
the intrinsics probe stopped encoding a UTF-16 string per name lookup (see
Performance). The full methodology and triage live in `.notes/conformance.md`.

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
(`node tools/corpus/bench.js` runs the workloads under Slag and V8, each with
its JIT and with it disabled — see `tools/corpus/README.md`). The numbers
below are one machine (AMD Ryzen 9 7950X, Windows 11, rustc 1.96.0, node
v24.12.0): medians of five runs for the micro-suite, one run for the corpus.
The release profile is part of the measurement, not an aside: `[profile.release]`
pins `codegen-units = 1` + `lto = "thin"`, because at cargo's default 16 codegen
units the same source built two ways is laid out differently and differs by up
to ~13% on the interpreter's own paths — a spread larger than any source change
these tables record. The standing detail, the benchmark gates, and the deferred
work live in `.notes/perf.md`.

### The JIT against the interpreter

Ratio < 1 means the JIT is faster than the interpreter (`result-ok` checks
both paths computed the same value):

| Body | Interpreter | JIT | Ratio |
|---|---|---|---|
| `arithmetic` | 7.90 ms | 0.671 ms | 0.08x |
| `wide leaf call` | 23.02 ms | 2.018 ms | 0.09x |
| `bare loop` | 7.39 ms | 0.772 ms | 0.10x |
| `property read` | 8.23 ms | 0.808 ms | 0.10x |
| `global read` | 10.47 ms | 1.512 ms | 0.14x |
| `typed-array read` | 96.48 ms | 14.522 ms | 0.15x |
| `builtin call` | 4.60 ms | 0.748 ms | 0.16x |
| `string concat` | 1.22 ms | 0.205 ms | 0.17x |
| `function calls` | 5.97 ms | 1.006 ms | 0.17x |
| `typed-array length` | 11.90 ms | 2.220 ms | 0.19x |
| `buildString shape` | 79.47 ms | 15.672 ms | 0.20x |
| `non-leaf call` | 28.11 ms | 6.394 ms | 0.23x |
| `typed-array write` | 28.11 ms | 6.518 ms | 0.23x |
| `buildString full` | 68.66 ms | 18.873 ms | 0.27x |
| `apply leaf call` | 20.59 ms | 7.318 ms | 0.36x |
| `compound assign` | 3.47 ms | 1.758 ms | 0.51x |

`builtin call` and `non-leaf call` are the two rows that cannot inline — a body
that contains a call is not a leaf, and a crux-native builtin is not a JS leaf —
so both engines run the general call path there and the ratio is set by that
machinery, not by code generation. `typed-array read` is the one row this suite
gained over `v0.1.0-preview.2`; the rest of the set is unchanged.
`.notes/perf.md` has the per-shape probe and what each row moved.

One row needs a caveat: `wide leaf call`'s interpreter column is the suite's
noisiest (±20% run to run), and it is the single row the pinned release profile
did not help — the suite measured it +21% and an isolated 3M-iteration probe of
the same shape +17%, while a later session could not separate the two profiles
at all. Every other row is stable in either direction, and the profile
decision rests on the column totals (interpreter −12%), not on that row.

### Against V8

The corpus runner checks parity as well as time: all four engine/mode
combinations return the same value for every workload (`mismatches 0`).
Gap = Slag ms / V8 ms, so > 1 means V8 was faster:

| Family | Workloads | JIT gap | Interpreter gap |
|---|---|---|---|
| arrays | 6 | 18.74x | 4.23x |
| builtins | 5 | 10.60x | 5.07x |
| calls | 6 | 13.02x | 5.63x |
| control | 5 | 20.17x | 5.66x |
| globals | 4 | 2.16x | 0.72x |
| language | 3 | 14.39x | 10.26x |
| objects | 7 | 87.57x | 3.22x |
| opcost | 36 | 24.20x | 4.81x |
| strings | 5 | 28.69x | 21.75x |
| **All** | **77** | **26.28x** | **5.86x** |

A sample of the per-workload rows (the command above prints all 77), ms per
`bench()` call:

| Workload | Slag JIT | Slag interp | V8 JIT | V8 `--jitless` | JIT gap | Interp gap |
|---|---|---|---|---|---|---|
| `arrays/for_of_dense.js` | 8.4 | 59.4 | 1.9 | 70.0 | 4.34x | 0.85x |
| `arrays/typed_array.js` | 34.1 | 201.1 | 1.2 | 42.4 | 29.02x | 4.74x |
| `builtins/json_roundtrip.js` | 73.1 | 71.5 | 13.6 | 17.2 | 5.37x | 4.15x |
| `builtins/math_intrinsics.js` | 11.3 | 68.8 | 170.5 | 200.9 | 0.07x | 0.34x |
| `calls/direct_leaf.js` | 13.4 | 77.9 | 1.2 | 40.2 | 10.75x | 1.94x |
| `calls/recursive_fib.js` | 54.9 | 574.8 | 7.8 | 37.1 | 7.00x | 15.48x |
| `control/generator_loop.js` | 126.2 | 140.7 | 2.5 | 10.4 | 50.17x | 13.57x |
| `globals/declarative_read.js` | 2.6 | 17.1 | 0.6 | 13.3 | 4.22x | 1.28x |
| `globals/hoisted_local.js` | 2.7 | 17.0 | 0.6 | 13.1 | 4.35x | 1.30x |
| `globals/nested_read.js` | 2.8 | 17.3 | 103.2 | 115.3 | 0.03x | 0.15x |
| `globals/object_read.js` | 2.8 | 18.0 | 94.2 | 106.9 | 0.03x | 0.17x |
| `objects/destructure.js` | 440.5 | 305.6 | 0.8 | 54.0 | 526.15x | 5.66x |
| `objects/own_read.js` | 3.1 | 32.5 | 1.4 | 52.8 | 2.25x | 0.62x |
| `objects/warm_store.js` | 74.9 | 111.3 | 2.7 | 57.4 | 28.02x | 1.94x |
| `strings/char_ops.js` | 12.8 | 24.3 | 0.4 | 5.8 | 30.22x | 4.17x |

The `globals` family is the one family that is a controlled experiment rather
than a workload: the same loop reading the same value as a top-level `const`
(`declarative_read`), hoisted into a local first (`hoisted_local`), read through
the global object (`object_read`), and read through the same global object from a
helper nested inside another function (`nested_read` — the shape a mod's kernels
have). All four still measure the same ~2.6 ms. Two gaps were closed there: the cell
was never warmed for the global env's declarative record (a top-level `const`
cost 52.5 ms — 21x), and it was never *probed* from a body whose env chain was
not the bare global record (a nested helper's global read cost 130.2 ms per 1M
reads — 50x). A declarative cell is load-only, and both stay valid because the
global environment bumps the global object's generation on every declarative
mutation, and because an in-place member write to a global refreshes the cell
(the in-place store deliberately does not bump). See
`.notes/frame-cost-profile.md` §4b.

Read the gaps as "where the work is", not as a verdict on the engine shape:
the corpus's own README records the workloads V8 folds or scalar-evolves to a
near-constant (which is why `destructure` and the other such rows have
outsized JIT ratios), and a sub-1 gap means Slag was faster on that workload.

### Against the previous release (`v0.1.0-preview.2`)

The same corpus and micro-suite were run against the `v0.1.0-preview.2` tag
(2026-09-26). The micro-suite is the cleaner comparison — both binaries print
`result-ok`, i.e. every row compiled on both — and there three rows moved 2x or
more while the rest are flat:

| Micro row | preview.2 JIT | now JIT | Change |
|---|---|---|---|
| `builtin call` | 2.523 ms | 0.748 ms | 3.4x faster |
| `non-leaf call` | 13.831 ms | 6.394 ms | 2.2x faster |
| `typed-array write` | 13.418 ms | 6.518 ms | 2.1x faster |
| `function calls` | 0.823 ms | 1.006 ms | ~flat (some slower) |
| `wide leaf call` | 1.832 ms | 2.018 ms | ~flat |
| every other row | — | — | within noise |

(`typed-array read` is new since preview.2: 15 rows there, 16 now.)

The corpus moved further, but its headline needs a caveat read first. Overall
the corpus means went from `mean-jitGap` 116.05 / `mean-jlGap` 9.19 to
**26.28** / **5.86** — yet that is dominated by the `opcost` family (36 of the
77 rows), and `tools/corpus/README.md` says a two-binary pair is a measurement
only if each binary's own control row is stable: *"If a binary's own control
row — `baseline` — has moved, the pair is not a measurement of anything."* It
moved here, 2.4 ms → 0.3 ms — a real ~8x gain on the `i & MASK` / `| 0` bare
loop — so the `opcost` per-row deltas are not a clean per-operation
attribution. Excluding `opcost`, the remaining 41 rows went from a JIT gap of
**34.81x to 28.11x** (≈1.24x closer to V8) and an interpreter gap of **6.31x to
6.77x** (about flat):

| Family | Workloads | JIT gap (preview.2 → now) | Interpreter gap (preview.2 → now) |
|---|---|---|---|
| arrays | 6 | 35.22x → 18.74x | 5.87x → 4.23x |
| builtins | 5 | 15.22x → 10.60x | 6.64x → 5.07x |
| calls | 6 | 24.25x → 13.02x | 5.00x → 5.63x |
| control | 5 | 26.74x → 20.17x | 5.45x → 5.66x |
| globals | 4 | 2.12x → 2.16x | 0.69x → 0.72x |
| language | 3 | 13.16x → 14.39x | 7.03x → 10.26x |
| objects | 7 | 90.32x → 87.57x | 3.24x → 3.22x |
| strings | 5 | 36.04x → 28.69x | 17.32x → 21.75x |
| opcost (see caveat) | 36 | 208.57x → 24.20x | 12.46x → 4.81x |
| **All** | **77** | **116.05x → 26.28x** | **9.19x → 5.86x** |
| **All, excluding `opcost`** | **41** | **34.81x → 28.11x** | **6.31x → 6.77x** |

Only one round was run per binary, so per-workload cross-binary rows carry the
±warm-up band the corpus README describes; the family and overall means are the
number to read. The shape: the JIT closed the most on arrays, calls and
builtins, the interpreter is essentially unchanged outside `opcost`, and the
`globals` family — already at parity — did not move.

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
| `v8` | Stand-in for `rusty_v8` (named `v8` at API version 150.4.0) that runs Deno on Slag; not in the default build (`cargo build -p v8`, or `python3 tools/build-deno.py` to build the whole Deno host) |
| `cli` | The `slag` binary (script runner + REPL) |
| `test262` | The pinned corpus + the sweep runner |

## Documentation

- `PLAN.md` — the implementation plan, per-phase spec coverage, and status
- `.notes/conformance.md` — conformance methodology, results, and triage
- `.notes/gc-plan.md` — the GC milestone plan (arena heap + mark-sweep, cut-by-cut)
- `.notes/barrier-uaf.md` — open: a write-barrier miss / suspected use-after-free in the promise/arguments path under `--gc-stress`
- `.notes/intl-plan.md` — the ECMA-402 (Intl) implementation plan and cut status
- `.notes/jit-report.md` — the experimental Cranelift JIT: design, fast paths, hardening, validation, and remaining work
- `.notes/memory-model.md` — ECMAScript ch. 28 shared-memory model
- `.notes/wasm-plan.md` — the WebAssembly engine plan: cuts, decisions, conformance status
- `.notes/perf.md` — performance milestones and deferred work

## License

MIT OR Apache-2.0
