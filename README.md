# Slag

> The stony waste matter separated from metals during the smelting or
> refining of ore. It's gritty, memorable, and definitely unconventional.

Slag is a from-scratch, spec-faithful JavaScript engine in Rust,
implementing the ECMAScript® 2026 Language Specification (17th edition). It
began as a spite project — when Bun refused to compile JavaScriptCore for
the author's OS, the author wrote a JavaScript engine from scratch instead.
It passes **100% of runnable `test262` conformance** across all three sweep
areas — `language`, `built-ins`, and Annex B — and ships a command-line
runner/REPL plus a small embedding API.

## Highlights

- **Spec-faithful** — written chapter-by-chapter against the vendored
  `spec.html`; abstract operations keep the spec's names, ordering, and
  edge cases so conformance bugs are easy to diff.
- **Conformant** — 36,187 passing fixtures, 0 failures across 48,622
  `test262` fixtures (runnable-only; see `docs/conformance.md`).
- **Portable** — no third-party runtime dependencies beyond the Rust
  standard library (see `PLAN.md` §4.10).
- **Embeddable** — `runtime::Context` with `JsValue`/`JsObject` handle
  types and `HostCallbacks` for host integration.

## Quick start

Requires a stable Rust toolchain (edition 2024).

```sh
cargo build --release
target/release/slag --version
target/release/slag script.js [args...]   # run a script
target/release/slag                       # REPL
```

The CLI exposes `process.argv` to scripts, and accepts `--dump-ast`,
`--dump-tokens`, and `--bench`; the `--print-bytecode`, `--stack-size`,
`--max-old-space`, and `--harmony-*` knobs are accepted for compatibility
(no-ops for now).

## Embedding

`Context` is the entry point — a fresh agent, realm, and host globals
(`console`, timers, `Math.random` override) per instance.

```rust
use runtime::embed::Context;

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

The `test262` crate vendors 3,317 fixtures as a regression gate and ships a
standalone sweep runner (`cargo run --release -p test262 --bin sweep`)
that runs any part of the full suite in parallel, timeout-guarded batches.
Current sweep result (release build, long config):

| Area | Total | Pass | Fail | Pass % of runnable |
|---|---|---|---|---|
| language | 23,724 | 18,052 | 0 | 100.0% |
| built-ins | 23,812 | 17,179 | 0 | 100.0% |
| annexB | 1,086 | 956 | 0 | 100.0% |
| **Total** | **48,622** | **36,187** | **0** | **100.0%** |

Skips are the module/async fixtures, host-dependent behavior, and the
out-of-scope Temporal/ShadowRealm/await-dictionary proposals; the full
methodology and triage live in `docs/conformance.md`.

## Repository layout

| Crate | Responsibility |
|---|---|
| `unicode` | Code-point tables, case conversion, ID_Start/ID_Continue |
| `crux` | `Value`, strings, property keys, completion records, GC handles |
| `syntax` | `SourceText`, `Span`, `Token`, the full AST |
| `lexer` | Tokenizer: lexical goals, comments, literals, ASI |
| `parser` | Recursive-descent parser, cover grammar, early errors |
| `regexp` | RegExp pattern parser + backtracking matcher |
| `runtime` | Realms, environments, evaluation, modules, all built-ins, `Context` |
| `cli` | The `slag` binary (script runner + REPL) |
| `test262` | Vendored conformance fixtures + the sweep runner |

## Documentation

- `PLAN.md` — the implementation plan, per-phase spec coverage, and status
- `docs/conformance.md` — conformance methodology, results, and triage
- `docs/memory-model.md` — ECMAScript ch. 28 shared-memory model
- `docs/perf.md` — performance milestones and deferred work

## License

MIT OR Apache-2.0
