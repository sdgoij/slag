# Slag

> The stony waste matter separated from metals during the smelting or
> refining of ore. It's gritty, memorable, and definitely unconventional.

> A test262 runner. It also happens to execute JavaScript.

Slag is a from-scratch, spec-faithful JavaScript engine in Rust,
implementing the ECMAScript® 2026 Language Specification (17th edition). It
began as a spite project — when Bun refused to compile JavaScriptCore for
the author's OS, the author wrote a JavaScript engine from scratch instead.

The full pinned `test262` corpus is the regression net: **48,006 pass /
0 fail / 0 crash** across the `language`, `built-ins`, and Annex B sweep
areas (a fourth `intl402` area runs the ECMA-402 fixtures). That includes
**proper tail calls** — the one spec feature V8 and JSC still skip —
the **Intl** surface (ECMA-402 Cuts 1–8: NumberFormat, Locale,
PluralRules, RelativeTimeFormat, ListFormat, DisplayNames, DateTimeFormat,
Collator, Segmenter, DurationFormat), and **Temporal**. It ships a
command-line runner/REPL, a small embedding API, and drop-in
JavaScriptCore C-API bindings.

## Highlights

- **Spec-faithful** — written chapter-by-chapter against the vendored
  `spec.html`; abstract operations keep the spec's names, ordering, and
  edge cases so conformance bugs are easy to diff.
- **Conformant** — 48,006 passing fixtures, **0 failures / 0 crashes**
  across 48,622 `test262` fixtures (runnable-only; see
  `docs/conformance.md`). Proper tail calls: 34/34 `tco-*`. Workspace
  tests: 4,316 pass / 0 fail.
- **Complete modern feature surface** — modules (source-text module
  machinery, top-level await, dynamic import), async/await, generators,
  Proxy/Reflect, TypedArrays, SharedArrayBuffer/Atomics with worker
  threads, full Intl (ECMA-402 Cuts 1–8), and Temporal (the intl402×
  Temporal integration, Cut 9, is in flight).
- **Experimental Cranelift JIT** — `--jit` compiles the interpreter's
  certified `Step` bytecode to native machine code via
  [Cranelift](https://cranelift.dev): inline number/string fast paths,
  direct-mapped global/member value cells, and register-resident fast
  loops. `--jit-bench` times JIT vs interpreter, and the conformance
  sweep runs the full corpus through the JIT by default (`--jitless`
  disables it). Design, status, and remaining work: `docs/jit-report.md`.
- **Portable** — no third-party runtime dependencies beyond the Rust
  standard library (see `PLAN.md` §4.10), with one opt-in exception: the
  experimental JIT (`crates/jit`) pulls in Cranelift and `region` and is
  installed only when the `--jit` flag is given. The Unicode property
  tables are generated at compile time from the pinned corpus fixtures,
  so they can never drift from what the tests assert.
- **Embeddable** — the `slag` crate exposes the Rust embedding API
  (`Context`, `JsValue`/`JsObject`, `HostCallbacks`, optional Cranelift JIT
  hook), plus a drop-in JavaScriptCore C API (`crates/jsc`).

## Quick start

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
`readdirSync`/`statSync`) to scripts, and accepts `--dump-ast`,
`--dump-tokens`, `--print-bytecode` (dump the compiled `Step` stream),
and `--bench`; `--jit` runs certified bodies through the experimental
Cranelift JIT and `--jit-bench` times JIT vs interpreter. The
`--stack-size`, `--max-old-space`, and `--harmony-*` knobs are accepted
for compatibility (no-ops for now).

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
`install_process_argv` installs a Node-style `process.argv`.

## Conformance

The pinned `test262` submodule is the regression net: the sweep runner
(`cargo run --release -p test262 --bin sweep`) runs any area in parallel,
timeout-guarded batches, and the `unicode` build script derives the
property-escape tables from the same fixtures. Current sweep result
(release build, default 15s deadline):

| Area | Total | Pass | Fail | Skip | Hang | Pass % of runnable |
|---|---|---|---|---|---|---|
| language | 23,724 | 23,721 | 0 | 3 | 0 | 100.0% |
| built-ins | 23,812 | 23,199 | 0 | 155 | 458 | 100.0% |
| annexB | 1,086 | 1,086 | 0 | 0 | 0 | 100.0% |
| **Total** | **48,622** | **48,006** | **0** | **158** | **458** | **100.0%** |

The skips are the out-of-scope `await-dictionary` (89) and `ShadowRealm`
(64) proposal fixtures, one stale Temporal fixture, and 4 fixtures this
Windows checkout cannot run: the submodule is checked out CRLF by
`core.autocrlf`, so their byte-exact assertions read `\r\n` where the
corpus asserts `\n` (the skip is conditional — a clean LF checkout runs
them). The 458 hangs are slow-but-correct fixtures at the default
deadline; the long config (`--timeout 120 --recheck-timeout 120`)
reclassifies them as passes. The full methodology and triage live in
`docs/conformance.md`; the honest market-readiness assessment is
`docs/readiness.md`.

## Repository layout

| Crate | Responsibility |
|---|---|
| `unicode` | Code-point tables, case conversion, ID_Start/ID_Continue, derived `\p{...}` tables (generated from the corpus at build time) |
| `crux` | `Value`, strings, property keys, completion records, GC handles |
| `syntax` | `SourceText`, `Span`, `Token`, the full AST |
| `lexer` | Tokenizer: lexical goals, comments, literals, ASI |
| `parser` | Recursive-descent parser, cover grammar, early errors |
| `regexp` | RegExp pattern parser + backtracking matcher |
| `runtime` | Realms, environments, evaluation, modules, all built-ins (incl. Intl + Temporal), `Context` |
| `jit` | Experimental Cranelift JIT backend for the interpreter's certified `Step` bytecode (opt-in via `--jit`) |
| `ffi` | Shared C-ABI plumbing for the drop-in surfaces (handle tables, value/string marshaling) |
| `jsc` | Drop-in JavaScriptCore C API (`JSContextRef` family) backed by Slag |
| `cli` | The `slag` binary (script runner + REPL) |
| `test262` | The pinned corpus + the sweep runner |

## Documentation

- `PLAN.md` — the implementation plan, per-phase spec coverage, and status
- `docs/conformance.md` — conformance methodology, results, and triage
- `docs/readiness.md` — honest readiness and market-entry assessment
- `docs/gc-plan.md` — the GC milestone plan (arena heap + mark-sweep, cut-by-cut)
- `docs/intl-plan.md` — the ECMA-402 (Intl) implementation plan and cut status
- `docs/jit-report.md` — the experimental Cranelift JIT: design, fast paths, hardening, validation, and remaining work
- `docs/memory-model.md` — ECMAScript ch. 28 shared-memory model
- `docs/perf.md` — performance milestones and deferred work

## License

MIT OR Apache-2.0
