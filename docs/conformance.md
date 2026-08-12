# Conformance

Status of ECMAScript conformance for the jsrt runtime, and how it is
measured (PLAN Phase 18, "Conformance hardening").

## How conformance is measured

Two harnesses live in `crates/test262`:

1. **Vendored fixtures** (the default `cargo test -p test262` suite): one
   `#[test]` per fixture, hand-selected per phase as built-ins and language
   features landed. Each fixture runs in both sloppy and strict mode (unless
   the frontmatter says otherwise), with the real test262 harness helpers
   where needed (`testAtomics.js`, `testTypedArray.js`) plus native
   equivalents for `assert`, `Test262Error`, `$ERROR`, and `isConstructor`.
2. **Full-suite sweep** (ignored test, opt-in): walks the entire pinned
   test262 submodule (`test/language`, `test/built-ins`, `test/annexB`) and
   triages every fixture into pass / fail / skip buckets, printing
   per-directory statistics and failure samples. Run it with:

   ```
   cargo test -p test262 --lib -- --ignored full_sweep --nocapture
   ```

   Set `SWEEP=language|built-ins|annexB` to scan one area, and
   `SWEEP_SAMPLE=N` to cap the run at N fixtures per top-level directory
   (the full suite is ~49,000 files; the sample keeps the triage shape
   representative in about a minute).

## Current results

Workspace-wide: **4055 tests pass, 0 failures** (`cargo test --workspace`),
of which the test262 crate contributes **3317 passing fixtures** (44
language-area + 3275 built-ins fixtures); the remaining registered test is
the ignored `scan_builtins_directories` directory scanner. The `workers`
feature build adds 444 runtime tests (`cargo test -p runtime --features
workers`).

The vendored fixtures cover, by phase: the execution model and language
syntax (Phases 4-6), functions/classes/generators/async/modules (Phase 7),
the global object and fundamental objects (Phase 8), numbers and dates
(Phase 9), String (Phase 10), RegExp (Phase 11), Array/TypedArray (Phase
12), Map/Set/WeakMap/WeakSet (Phase 13), ArrayBuffer/DataView/Atomics/JSON
(Phase 14), iterators/generators/async/promises (Phase 15), and
Proxy/Reflect (Phase 16). Phase 17's Atomics and worker changes added
runtime-side unit tests; the Atomics fixture directories (`Atomics/`, 144
fixtures) pass.

## Edge-case unit-test campaign (Phase 18 hardening)

Beyond the vendored fixtures, ~120 edge-case unit tests were added across
the crates (lexer, parser, runtime core, and the built-ins), targeting the
plan's per-phase test lists: numeric-literal/escape/ASI lexing, the ASI ×
statement matrix and cover grammar, TDZ/hoisting/redeclaration/eval
scoping, `-0`/`NaN`/`2^53` conversion boundaries, case-mapping expansion,
split/replace substitution patterns, holes/length-mutation/species/sort,
and buffer resize/transfer/detach semantics. `array_buffer.rs` and
`dataview.rs` received their first unit tests.

The campaign surfaced and fixed the following conformance bugs (each now
has a regression test):

- **Lexer:** escaped `$`/`_` could not start an identifier (`\u0024x`); the
  escape branch now validates with the full IdentifierStart/Part predicate.
- **Parser:** rest-element-must-be-last was not enforced in assignment
  targets, array arrow parameters, or `for-of` heads (`[...a, b] = c`); the
  catch-parameter redeclaration rules were wrong — `catch (e) { var e; }`
  and `var e; … catch (e)` were rejected while `catch (e) { let e; }` was
  accepted. Both directions now follow spec 15.1.8.
- **For-head declarations:** `let`/`const` for-heads were instantiated as
  global `var`s, so `for (let i …)` leaked `i` onto `globalThis` and
  `for (let x = x;;)` missed the TDZ.
- **String:** `String(Symbol('x'))` threw instead of returning
  `"Symbol(x)"`; the Unicode Final_Sigma conditional mapping
  (`'ΟΣ'.toLowerCase()` → `"ος"`) was missing.
- **RegExp:** `RegExp.prototype[@@split]` result arrays lacked
  `%Array.prototype%` (`.join` on them failed).
- **Number:** `Number('0b101')`/`Number('0o17')` returned NaN; binary and
  octal StringNumericLiterals are now parsed.
- **Array:** the `@@species` accessor lived on `%Array.prototype%` instead
  of the `Array` constructor (`Array[Symbol.species]` was `undefined`), and
  `ArraySpeciesCreate` ignored custom `@@species` overrides.
- **TypedArray/ArrayBuffer:** self-allocated views' backing buffers carried
  `%Object.prototype%` (so `buffer.byteLength` was `undefined`); reads and
  writes through views of a detached buffer returned stale data instead of
  throwing; `transfer()` on an already-detached buffer returned it instead
  of throwing.

## What is skipped and why

The harness's skip taxonomy (also used by the sweep):

| Skip category | Reason |
|---|---|
| `flags: module` | No module loader: `import`/`export` parse, but linking, `dynamic import`, and `import.meta` are host-dependent (see below). |
| `flags: async` | The `$DONE` async harness is not provided; async semantics are covered by the runtime's own async test suites. |
| Unsupported `includes:` | Fixtures needing harness helpers beyond `assert.js`, `compareArray.js`, `detachArrayBuffer.js`, `isConstructor.js`, `propertyHelper.js`, `testAtomics.js`, `testTypedArray.js` are not run. |
| Intl directories | `Intl` (ECMA-402) is out of scope for this runtime (PLAN scope decision). |

## Expected non-runnable tests

These categories are expected to stay non-runnable and are not counted
against the runnable pass-rate target:

- **Intl-required features** — ECMA-402 is a separate specification.
- **`dynamic import` without a module loader** — `import(specifier)`
  resolves through host hooks; no loader is shipped, matching the plan's
  "no module loader" carve-out.
- **Host-dependent behavior** — e.g. `Atomics.wait` on the main thread
  (throws, per spec `[[CanBlock]] = false`), timers (host globals, provided
  by the embedding API), `SharedArrayBuffer` worker creation (host hook).

## Annex B

Annex B legacy behaviors are part of the spec and are tracked explicitly:
the parser implements Annex B HTML comments, legacy octal literals, the
`for-in` initializer extension, and the Annex B `RegExp` legacy features,
each with unit tests. The full `annexB/` sweep is available via the sweep
tool above.

## Error messages and stack traces

Error objects carry a `message` and a V8-style `stack` captured at
construction (see `crates/runtime/src/builtins/error.rs`). `message` text is
engine-defined; the goal is V8-compatible phrasing for the common built-in
errors, with exact wording verified fixture-by-fixture. `stack` follows the
V8 shape (`ErrorType: message\n    at …`) with source spans from the parser.

## Open items

- The full ~49k-fixture sweep has not been completed in this phase (it
  takes ~10 minutes unbounded). Run it (bounded with `SWEEP_SAMPLE`) and
  record the per-directory triage here to certify the ≥95%-of-runnable
  target from the plan.
- `Intl` fixtures are excluded by design; anything else that fails the
  sweep should be triaged into bug / host-dependent / missing-hook
  categories and either fixed or documented here.
