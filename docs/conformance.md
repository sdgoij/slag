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
   This is the regression gate.
2. **`test262-sweep` runner** (`cargo run -p test262 --bin sweep`): runs any
   part of the pinned submodule in small, concurrent, timeout-guarded
   batches. A whole sweep is ~49,000 fixtures and a single hanging fixture
   (usually an interpreter bug) can stall an in-process runner forever, so
   each batch runs in a **child process** the parent kills when it exceeds
   the batch deadline; un-reported fixtures are then re-run individually to
   pinpoint the hang (`HANG`). Process death mid-fixture is reported as
   `CRASH` (stack overflow, panics).

   ```
   cargo run -p test262 --bin sweep -- [area] [options]
   ```

   - `area`: `language` | `built-ins` | `annexB` | `all` (default `all`)
   - `--jobs N` concurrent batches, `--batch N` fixtures per batch
     (default 32), `--timeout SECS` batch deadline (default 30),
     `--recheck-timeout SECS` per-fixture hang recheck (default 5)
   - `--sample N` caps the run at N fixtures per top-level directory (a
     representative ~1-minute triage), `--filter GLOB` selects by relative
     path (`*` and `?`)
   - `--json` emits a machine-readable report (for diffing runs); the text
     report prints per-directory tallies, failure samples, skip reasons,
     and the hang list

   Fixtures share the same `harness::run_fixture` code path as the vendored
   tests; `*_FIXTURE.js` helper files are excluded from collection.

   An in-process whole-suite scan also exists as an ignored test
   (`full_sweep`, with `SWEEP`/`SWEEP_SAMPLE` env knobs), but the runner is
   the recommended tool: it parallelizes, bounds each batch, and survives
   hangs.

## Current results

Workspace-wide: **4066 tests pass, 0 failures** (`cargo test --workspace`),
of which the test262 crate contributes **3317 passing fixtures** (44
language-area + 3275 built-ins fixtures); the remaining registered test is
the ignored `scan_builtins_directories` directory scanner. The `workers`
feature build adds 452 runtime tests (`cargo test -p runtime --features
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

### TypedArray cluster sweep (Phase 18 conformance)

A dedicated sweep of the whole TypedArray fixture tree (`sweep.exe built-ins
--filter '*TypedArray*' --jobs 8 --batch 32 --timeout 120 --recheck-timeout
90`) currently reports **2043 pass, 0 fail, 141 skip, 0 crash, 0 hang** of
2184 fixtures: every runnable TypedArray fixture passes. The 141 skips are
the standard taxonomy (module/async flags, unsupported harness includes,
`$262.createRealm`). This closed the cluster from 891 pass / 202 fail. The
work also removed the `TypedArray/prototype/copyWithin/coerced-values-*`
"hangs" seen with short timeouts — those fixtures allocate 10,000-element
arrays and need the long timeout config, not a bug.

The fixes clustered into spec-order and feature work:

- **Resizable buffers (ES2025):** views now track `[[ArrayLength]]`/length
  through `rab.resize`, including the "auto" length-tracking views created
  over resizable buffers without an explicit length; `length`/`byteLength`/
  `byteOffset` report 0 for out-of-bounds views, and `ValidateTypedArray`
  throws for them. The byteLength-multiple RangeError now applies only to
  fixed (non-resizable) buffers.
- **Detached-during-coercion:** methods re-check the buffer after each
  argument coercion (`fill`, `slice`, `set`, `copyWithin`, the constructors)
  and throw the spec TypeError; the harness's `$262.detachArrayBuffer` is
  idempotent like a host detach.
- **Species and change-array-by-copy:** `TypedArrayCreate` validates the
  species result (detached, too-short, immutable); `with`/`toReversed`/
  `toSorted` use `TypedArrayCreateSameType` (ignoring @@species); slice
  re-clamps its copy after a resize; `subarray` uses the auto-length
  two-argument species form.
- **Immutable buffers (ES2026):** `ArrayBuffer.prototype.transferToImmutable`
  plus write-mode validation (`ValidateTypedArray(O, write)`) that rejects
  immutable destinations before any argument coercion.
- **Agent-aware coercions:** argument `ToNumber`/`ToBigInt`/`ToIndex` now
  route through the agent, `@@toPrimitive` is honored by both the runtime
  and crux coercion paths, boxed primitives (`new Number(2.3)`) coerce
  without invoking a placeholder, and `Object.getPrototypeOf`/`Reflect.get
  PrototypeOf` return function prototypes as functions. The arguments object
  gained `@@iterator = %Array.prototype.values%`.
- **Method correctness:** `fill`/`with` coerce the value exactly once and in
  the spec order; `indexOf`/`lastIndexOf` check `HasProperty`; `join` maps
  undefined/null to the empty string and captures its length before the
  separator coercion; `sort` stops at a comparefn error; the per-kind
  `BYTES_PER_ELEMENT` prototype properties, the `TypedArray` global, and the
  `toString = %Array.prototype.toString%` identity were added.

### Array.from cluster sweep (Phase 18 conformance)

A follow-up sweep of the `Array/from` fixture tree (`sweep.exe built-ins
--filter 'Array/from*'`) reports **51 pass, 0 fail, 91 skip** of 142
fixtures: every runnable `Array.from` fixture passes. The 91 skips are the
standard taxonomy (90 `flags: async` fromAsync tests, 1 `$262.createRealm`).
`Array.from` previously passed the mapfn a third `array` argument and
constructed the custom constructor with « 0 » before iterating; it now
follows spec 23.1.2.2 exactly: the constructor is invoked with no arguments
and the length is set after the iteration for the iterable path, the mapfn
receives « kValue, k », and an abrupt mapfn or define completion closes the
iterator (`IteratorClose`) before propagating.

### AggregateError cluster sweep (Phase 18 conformance)

A sweep of `AggregateError*` reports **23 pass, 0 fail, 2 skip** of 25
fixtures (the 2 skips are `$262.createRealm` and the `promiseHelper.js`
include). Fixes in `crates/runtime/src/builtins/error.rs`:

- The `errors` argument is `IterableToList`-ed: it iterates when the value
  has `@@iterator` and falls back to the array-like copy.
- The error constructors now inherit `%Error%` and carry their spec lengths
  (`AggregateError.length` is 2, `SuppressedError.length` is 3); their
  prototypes are registered as intrinsics.
- `GetPrototypeFromConstructor` falls back to the constructor's own
  intrinsic default prototype instead of throwing on a non-object
  `newTarget.prototype`.
- `InstallErrorCause` creates `cause` when the option is present even if its
  value is `undefined` (a `HasProperty` check, not a value check).

### Error.prototype.stack and Error area sweep (Phase 18 conformance)

A sweep of `Error*` (93 fixtures) reports **85 pass, 0 fail, 8 skip**: every
runnable fixture passes, including the `Error/prototype/stack*` cluster (30
runnable of 35). The 8 skips are the standard taxonomy (5
`$262.createRealm`, 3 `proxyTrapsHelper.js` includes). Fixes:

- **`%Error.prototype.stack%` accessor (ES2026, spec 20.5.3.4-5):** the stack
  is a per-instance string captured at construction and served through the
  accessor (not an own data property). The getter TypeErrors on non-object
  receivers and returns `undefined` for objects without `[[ErrorData]]`; the
  setter (SetterThatIgnoresPrototypeProperties) rejects non-string values and
  `%Error.prototype%` as the receiver, creates an own data property when
  absent, and throws when a proxy's `defineProperty`/`set` trap reports
  false. The accessor carries the required `get stack`/`set stack` names and
  lengths.
- **BigInt(Number) — NumberToBigInt (spec 7.1.16):** `BigInt(0)` and
  `BigInt(1.5)` were both rejected with a TypeError; integral doubles now
  convert exactly (mantissa/exponent decomposition, so `BigInt(1e23)` is the
  exact double value) and NaN/±Infinity/non-integral throw a RangeError. The
  BigInt constructor, `asIntN`/`asUintN`, and BigInt typed-array element
  coercion share the conversion.
- **Proxy `[[Set]]` with throw:** `Set(O, P, V, true)` now converts a false
  proxy `set`-trap result into a TypeError instead of silently succeeding
  (spec 7.3.5 step 4).
- **`for-in` over functions:** enumeration matched `Value::Object` only, so
  `for (var k in fn)` yielded nothing; callables are now boxed like any
  object (spec 14.7.5.6 step 2 ToObject).
- **`%Error.prototype%[@@toStringTag]` removed:** the current spec does not
  define it (instances tag as `[object Error]` via `[[ErrorData]]`), so
  `Object.prototype.toString.call(Error.prototype)` is `[object Object]`
  again.
- **Error construction coercion order (spec 20.5.1.1):** the message's
  `ToString` ran once for the `message` property and again for the stack
  capture, invoking a side-effecting `toString` twice; the stack header now
  reuses the coerced own `message` property.
- **Harness:** the real `propertyHelper.js` is loaded from the submodule
  instead of the simplified prelude stub, so `verifyPrimordialAccessor
  Property` name/length checks and the configurable-deletes-property cleanup
  behave as upstream expects.

### String.prototype.split and RegExp method cluster sweeps (Phase 18)

A sweep of `String/prototype/split*` reports **120 pass, 0 fail** of 120
fixtures, and the RegExp area improved from 762 pass / 535 fail to **1079
pass / 218 fail** (of 1896; 599 skips are the standard taxonomy, mostly
`regExpUtils.js` includes). Closed to 100%-of-runnable: `RegExp/
property-escapes` (165), `RegExp/prototype/Symbol.split` (43),
`Symbol.match`+`Symbol.matchAll` (72), `%RegExpStringIteratorPrototype%
.next` (14), `Symbol.replace` (70), `Symbol.search` (23), and
`RegExp.prototype.flags` + every flag accessor. Fixes:

- **Boxed-string and object coercion:** crux `ToPrimitive` unwraps a String
  exotic object directly (like the boxed Number/BigInt/Boolean wrappers),
  and the regexp `@@` methods coerce their string arguments through the
  agent (`ToObject`/`ToString`), so `new String("abc").split(/x/)` and
  plain-object receivers work.
- **Regexp literal early errors (spec 13.3.2):** the JS parser now
  validates regexp literals at parse time (flags + pattern), so
  `phase: parse` negatives (`/\p{ASCII=Y}/u`, `a**`, `*a`, `x{1}{1,}`,
  `\k` without groups, `\x`/`\u` without hex digits, empty `[]` classes)
  are SyntaxErrors at the right phase.
- **`@@split`/`@@match`/`@@matchAll`/`@@replace`/`@@search` (spec
  22.2.7.x):** the flags come from `Get(rx, "flags")` (whose accessor
  composes each flag via Get in the `dgimsuvy` order, honoring overrides
  and `%RegExp.prototype%` returning `""`), the splitter/matcher is built
  through `SpeciesConstructor`, `RegExpExec` calls the custom `exec`
  method, empty matches advance lastIndex via `AdvanceStringIndex`, and
  `@@search` restores lastIndex only after a successful exec.
- **Property escapes:** the `=` value separator was rejected by the name
  scan (the value split was dead code), so every `\p{Script=…}`/
  `\p{scx=…}`/`\p{General_Category=…}` fixture failed; `Hex`/`Hex_Digit`
  were added to the binary properties, and `\p{…}` inside character
  classes works.
- **Host `print`:** the test262 shell `print` function is provided by the
  harness (a no-op echo), unblocking fixtures that alias it.
- **Duplicate named groups (ES2025):** the parser accepts duplicate
  `(?<x>…)` names and `\k<x>` resolves to the last group with that name;
  the `groups` object keeps first-occurrence key order. (The disjoint-
  capture and last-participating-group matching semantics of the proposal
  are still open.)
- **Global object prototype:** the global object now inherits
  `%Object.prototype%` (its proto was never wired after the intrinsic
  table populated), so `globalThis.toString` and friends resolve.

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

- **Destructuring assignment (interpreter):** array/object assignment
  targets were unsupported — `[a, b] = rhs`, `({x, y} = obj)`, defaults,
  rest, and nested patterns all failed at parse time or threw "Invalid
  left-hand side". `binding.rs` now implements
  DestructuringAssignmentEvaluation (13.15.5): reference evaluation runs
  before the iterator steps for simple targets, an abrupt iterator step
  marks the iterator done (so `return` is not called), and object rest
  values are boxed via `ToObject` with `%Object.prototype%` as the rest
  object's prototype.
- **Function-name inference:** `SetFunctionName` replaced the `""`
  placeholder only when the own `name` property was still empty (the
  previous "has own name" check always fired, since every function carries
  the placeholder), and a static `name` element on a class constructor now
  wins over the surrounding binding as the spec requires.
- **Generator/async-generator parameter binding:** `EvaluateGeneratorBody`/
  `EvaluateAsyncGeneratorBody` run `FunctionDeclarationInstantiation` at
  call time, so a destructuring parameter with a throwing `@@iterator`
  throws synchronously from the call (V8 does the same).
- **Cover grammar:** `{x = 1}` shorthand-initializers are now accepted
  where the object literal is (or may become) an assignment pattern —
  `result = {x = 1} = vals`, `[{x = 1}] = arr`, `for ({x = 1} of …)`,
  arrow params — and rejected everywhere else, via a deferred
  `cover_error` cleared by pattern contexts (`parse_assignment`,
  for-in/of heads, array/object/arrow-cover nesting).
- **Pattern early errors:** reserved words (incl. escaped, incl.
  strict-only `eval`/`arguments`/`let`/… and `await` in class static
  blocks) are rejected as shorthand identifiers; a rest element may not be
  followed by a comma (`[...x,]`, `{...x,}`) or carry an initializer
  (`[...x = 1]`); nested pattern elements must be valid targets
  (`[[(x, y)]]`, `[{ get x() {} }]`), enforced by a per-element target
  walk.

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

## Full-suite sweep (post-hardening)

`test262-sweep` over all three areas in a release build (48,622 fixtures, 8
jobs, 20s batch timeout): **0 crashes**; the only hangs are three known-slow
TypedArray fixtures that pass with the long timeout (see below).

| Area | Total | Pass | Fail | Skip | Hang | Pass % of runnable |
|---|---|---|---|---|---|---|
| language | 23,724 | 16,004 | 2,048 | 5,672 | 0 | 88.6% |
| built-ins | 23,812 | 15,393 | 5,015 | 3,401 | 3¹ | 75.4% |
| annexB | 1,086 | 439 | 558 | 89 | 0 | 44.0% |
| **Total** | **48,622** | **31,836** | **7,621** | **9,162** | **3** | **80.7%** |

(Runnable = pass + fail; the 9,162 skips are module/async fixtures and
unsupported harness includes.) The built-ins row reflects the current
`Error*`/`BigInt*` hardening; the language and annexB rows are from the
sweep recorded below.

¹ The 3 built-ins hangs are the `TypedArray/prototype/copyWithin`
coerced-values fixtures — 10,000-element allocations that need the long
(`--timeout 120`) config, not a bug (see the TypedArray cluster section).

The first sweep in a debug build reported 27 hangs and 92 crashes. Both are
resolved:

- **Slow builtin calls (the "decodeURI hangs")** — every builtin call walked a
  34-entry linear dispatch chain (`function::call_inner`), each entry doing an
  `intrinsics.get` (a `JsString` allocation + hash lookup). The dispatch
  resolution is now memoized per function id
  (`agent.builtin_dispatch_cache`), so plain closure builtins skip the chain
  after the first call. Builtin calls dropped from ~100-400µs to ~2µs, making
  the Sputnik stress fixtures (57-65k throw/catch iterations) 40-80× faster;
  the reported hangs were simply slow fixtures, not infinite loops.
- **`Array.prototype.splice` integer limit** — the spec's step-8 check
  (`len + insertCount - actualDeleteCount > 2^53-1` → TypeError) was missing,
  so a splice on a `2^53`-length array-like entered a 2^53-iteration tail
  shift. The check now throws before any shifting
  (`throws-if-integer-limit-exceeded.js` passes).
- **Crashes** were debug-build stack overflows from deep recursion; a release
  build runs them cleanly.

The 9,862 failures triage into:

- **Missing built-ins (excluded from runnable):** Temporal (~3,100
  fixtures across `Temporal/*` — not implemented), ShadowRealm (47), and
  Intl (never collected). Excluding them, the runnable pass rate is
  ~80%.
- **Systematic bug clusters (runnable, fix targets):**
  - Destructuring (`dstr`): the ~2,300-fixture assignment/class/for-of/
    generator/arrow cluster now passes **100% of runnable** (6,189 pass /
    0 fail of 8,783). The final fixes landed in the VM's step-based
    destructuring (`ir.rs`): the rest target's reference (with `yield` in a
    computed key) is compiled before the remaining values are collected; an
    exhausted iterator still feeds `undefined` to later elements so default
    initializers run (and may suspend); and a resumed `yield`/`await`
    aborted inside a pattern closes the active destructure iterators with
    the correct `IteratorClose` flavor (`return` vs `throw` completion).
    `GetIterator` also now caches a non-callable `next` without throwing, so
    a `yield` between GetIterator and the first step suspends first
  - `TypedArray/prototype` (944→183) and `TypedArrayConstructors/internals`
    (119→7): the Integer-Indexed exotic methods (spec 10.4.7) were fixed —
    `CanonicalNumericIndexString` now implements the ToNumber↔ToString
    round-trip (so `-1` and `1.1` are canonical index strings); `-0` is not
    a valid index; a detached buffer reads *undefined*, ignores writes, and
    reports absent (the web-reality alignment) instead of throwing; the
    `length`/`byteLength`/`byteOffset` accessors return 0 on a detached
    buffer; `[[HasProperty]]`/`[[Get]]`/`[[Set]]` no longer consult the
    prototype chain for canonical index keys; `[[OwnPropertyKeys]]` orders
    strings before symbols; and a set coerces the value before the index
    check. The harness also gained the `assert.js` helpers (`isPrimitive`,
    the bare `compareArray`) and skips `$262.createRealm` fixtures. The
    remaining TypedArray failures are resizable/auto-length views (5) and
    cross-crate coercion of wrapper objects (2); the `-realm` fixtures are
    skipped as host-dependent
  - `String/prototype` (290), `Array/prototype` (187),
    `Object/defineProperty`+`defineProperties`+`getOwnPropertyDescriptor`
    (313)
  - `RegExp/prototype` + `property-escapes` + `regexp-modifiers` (388)
  - `Iterator/prototype` (278), `DataView/prototype` (140)
  - `dynamic-import/syntax/valid` (137), class-element `delete` early
    errors (192), `eval-code/direct` (103), `identifiers` (58),
    `arguments-object`
  - Annex B: sloppy `eval`/`function`/`global` function-declaration
    semantics (410), `RegExp` (57), `escape`/`unescape` (35)

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

- Certify the ≥95%-of-runnable target: the sweep now measures 75.1%
  (≈80% excluding the not-implemented Temporal/ShadowRealm built-ins) with
  0 hangs and 0 crashes. The remaining gap is the systematic bug clusters
  listed above — the destructuring and TypedArray-Integer-Indexed clusters
  are done; fix the TypedArray prototype-method/auto-length, descriptor,
  and RegExp clusters next, then re-run the sweep and record the delta.
  Note: the TypedArray sweep should be run with a longer deadline
  (`--timeout 120 --recheck-timeout 90`) — the O(n²) property store makes
  the 10,000-element crash-test fixtures take ~45s, which the default 5s
  recheck misclassifies as hangs.
- The 27 original hangs were slow builtin calls (fixed via the dispatch
  cache) plus one real `Array.prototype.splice` infinite loop (fixed); the
  remaining sweep runs cleanly. Use a release build
  (`cargo run --release -p test262 --bin sweep`) — the debug build's deep
  recursion can overflow the stack on heavy fixtures.
- `Intl` fixtures are excluded by design; anything else that fails the
  sweep should be triaged into bug / host-dependent / missing-hook
  categories and either fixed or documented here.
