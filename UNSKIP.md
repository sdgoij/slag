The fun way it is. Let me first understand the CLI and how host globals get installed:

Now let me see how the runtime creates native host functions — reading `embed.rs` around `install_process_argv` and `install_random_override`:

The pattern is clear. Let me check the exact `Function::create_builtin` signature and the `ErrorKind` variants:

`NativeFn` takes `(&Value /* this */, &[Value] /* args */)` — and builtins reach the agent via `current_agent_mut()`. Let me confirm that accessor and `JsString::to_string_lossy`:

`NativeFn` is `Fn(&Value, &[Value])` and builtins pull the agent from `current_agent_mut()`. Let me check embed.rs's imports so I use the right type names:

I have the full picture. Now implementing — first the `install_fs` method in `embed.rs` (after `install_process_argv`). Let me check the end of embed.rs for the test module location first:

Now I'll make the edits. First the module doc comment and the `install_fs` method:

Now the `string_arg` helper — placing it before `console_output`:

Now a test in the embed tests module — let me see its current state:

Now the test, the CLI hookup, and the JS tally script:

Now let me build and check for errors:

One warning — an unused `agent` binding in the `statSync` closure (I don't need the agent there). Let me fix it:

Now clippy + fmt, then the release build and the tally run:

Clean. Now the release build and the dogfood run:

IT WORKS — Slag just classified its own test suite! And the numbers match the documented sweep almost exactly... except one fixture: built-ins shows **6,632** skips vs the doc's **6,633**. One fixture my classifier calls runnable that `run_fixture` skips. Let me diff against the real parser:

Found it — the Rust parser splits frontmatter on **both** `\n` and `\r` (CR-only fixtures exist, e.g. the toString line-terminator tests), while my JS only splits on `\n`. For a CR-only fixture, my classifier misses the flags entirely. Fixing:

**Exact match with the documented sweep: 5,672 / 6,633 / 130 skips — 36,187 runnable.** Let me grab the full breakdown to see where the CR-only fixture landed:

Full agreement — 36,187 runnable, 12,435 skipped, exact match with the documented sweep (the CR-only fixture landed in the `nativeFunctionMatcher.js` bucket). Now let me run the full test suite to confirm the new `fs` test passes and nothing regressed:

All green. Let me confirm the new fs test specifically ran and passes:

The fs test passes. Now let me delete the Python script and update the README's CLI globals mention:

Let me check if the Python script is still there:

Python shame erased (the file's already gone — I checked). Here's the full picture:

## The dogfood win 🍻

`target/release/slag skip_tally.js` — Slag itself walked 48,622 fixtures, parsed every frontmatter, and classified the skips. Result: **exactly** matches the documented sweep — 36,187 runnable / 12,435 skipped, every category. The one off-by-one turned out to be a CR-only fixture (the Rust parser splits frontmatter on both `\n` and `\r`; fixed the JS to match). The engine just ate its own test suite for lunch.

## What changed

- `crates/runtime/src/embed.rs` — new `Context::install_fs()`: minimal Node-style `fs` (`readFileSync`/`readdirSync`/`statSync`) + a test (`fs_reads_files_and_directories`, passing; 4158+ tests still green, clippy clean).
- `crates/cli/src/main.rs` — `run_file` installs `fs` for scripts.
- `README.md` — CLI globals sentence now mentions `fs`.
- `skip_tally.js` (repo root) — the dogfooded tally tool.

## The 6,784 skips, decoded

Re-tally after the async harness, `$262.createRealm`/`IsHTMLDDA`, the
harness-include work, and the Float16/resizable-buffer clusters:
`skip_tally.js` now mirrors `run_fixture` exactly, so the 5,468 async
fixtures, the createRealm/IsHTMLDDA/newly-enabled-include fixtures, and the
204 `byteConversionValues` + `resizableArrayBufferUtils` fixtures count as
runnable (they run and pass). **42,042 runnable / 6,580 skipped** — matching
the 0-fail sweep per area (22,634 / 18,322 / 1,086).

**Worth un-skipping, ordered by bang-for-buck (all done):**
| Target | Count | Effort |
|---|---|---|
| ~~`$262.createRealm`~~ | ~~190~~ | ✅ **Done** — host hook plus realm-aware builtin dispatch, `GetFunctionRealm` fallbacks, cross-realm error realms, proxy trap arrays/descriptors. All 190 run and pass. |
| ~~`$262.IsHTMLDDA`~~ | ~~34~~ | ✅ **Done** — `ObjectKind::IsHTMLDDA` (typeof "undefined", falsy, callable→null, `==` null/undefined) + `$262.IsHTMLDDA` host object. All 34 pass. |
| ~~Harness includes (easy)~~ | ~~~125~~ | ✅ **Done** — `decimalToHexString` (81), `nans` (12), `compareIterator` (13), `assertRelativeDateMs` (6), `iteratorZipUtils` (6), `dateConstants` (4), `deepEqual` (3) enabled and loaded from the submodule; +113 fixtures (36,524 / 0). Also fixed String static [[Prototype]] (fromCharCode/fromCodePoint had null proto → no `.call`/`.apply`). |
| ~~promiseHelper / proxyTrapsHelper / fnGlobalObject~~ | ~~~127~~ | ✅ **Done** — `Promise.any` passes the result capability's `[[Resolve]]` directly as each element's on-fulfilled (spec 27.2.4.4) and `AggregateError` runs message ToString → InstallErrorCause → errors iteration (20.5.7.1); `proxyTrapsHelper` enabled (28 fixtures) including a real spec-order fix in `Array.prototype.reverse` (DeletePropertyOrThrow before Set, 23.1.3.25 step 5.10); Annex B.3.3.3 hoist now uses `CreateGlobalVarBinding` so a pre-existing non-enumerable global property is left in place (+88 annexB global-decl fixtures). +127 fixtures (36,651 / 0). |
| ~~nativeFunctionMatcher / wellKnownIntrinsicObjects~~ | ~~~76~~ | ✅ **Done** — parser function/method spans start at the `async`/`*`/computed-name prefix so `Function.prototype.toString` returns the exact source; builtins created off-table (typed-array species getter, Number statics, Map/Set/Generator iterator `@@iterator`, `Math.random` host override) now link `%Function.prototype%` per CreateBuiltinFunction. +76 fixtures (36,727 / 0). |
| ~~`byteConversionValues`~~ | ~~19~~ | ✅ **Done** — Float16: `crux::typed_array::f16_from_f64` rounds directly from the full 53-bit f64 mantissa (the `half` crate goes via f32 on F16C and the fallback truncates low mantissa bits — both double-round at the 2⁻²⁵ subnormal boundary), and the subnormal decode is `fraction × 2⁻²⁴`. `Math.f16round` and the `DataView.setFloat16` paths agree with V8 at the subnormal boundaries. All 19 run and pass. |
| ~~`resizableArrayBufferUtils`~~ | ~~185~~ | ✅ **Done** — resizable-buffer TypedArray semantics (spec 10.4.5 + 25.2.3): `[[PreventExtensions]]` rejects length-tracking views and resizable-backed fixed views (Object.freeze throws, 10.4.5.1); `fill`/`copyWithin`/`slice` re-validate the view after argument coercion and throw TypeError when a shrink pushes a fixed-length view out of bounds; `copyWithin` skips source/dest elements that fell off the resized buffer; `slice` keeps the full count with fresh-allocation zeros beyond the live bytes; `map` captures its length before the species constructor runs; `set` and the typed-array→typed-array constructor use the current effective byte length for auto views; `Array.prototype.join` reads its length before coercing the separator; the Array Iterator throws TypeError for out-of-bounds views. All 185 run and pass. |
| ~~`async` ($DONE harness)~~ | ~~5,468~~ | ✅ **Done — 100% of runnable** — `$DONE`/`asyncTestPassed`/`asyncTestError` prelude + job-queue completion check; async-body errors (initial run, resumed body, parameter-binding defaults) reject the function's promise instead of throwing synchronously. The **dynamic-import cluster is done** (harness registers sibling `*_FIXTURE` modules from the submodule recursively; module env now chains to the global env; export-binding order fixed; namespace exotic object completed: live descriptors, sorted keys, `@@toStringTag`, define/delete semantics). The **async-generator driver was rewritten to the current spec** (the `draining-queue` model, 27.9.3): prototype methods create the capability before validating `this` (bad generators reject the promise instead of throwing synchronously — all 6 `this-val-not-*` fixtures); `yield` awaits its value with the state staying `executing` (queued requests wait, fixing the `request-queue-*` ordering); `AsyncGeneratorYield` completes the current request and continues without suspending when more requests are queued; `return()` values are awaited before reaching the body (`AsyncGeneratorUnwrapYieldResumption`, fixing the broken-promise fixtures) and on suspended-start/completed via `AsyncGeneratorAwaitReturn`; completed generators drain the queue. The async **`yield*` delegation was completed in the VM**: a done return result completes the body with a return completion of its value, a done next/throw result continues the body with the value; the awaited inner result must be an object (post-await check — a non-object's `then` is never consulted); iterator-result getter errors are catchable by the body; a delegation with no `return` method awaits the received value (new `AwaitReturn` suspension) and one with no `throw` method closes and throws the protocol-violation TypeError; `return expr` in an async generator awaits its value before completing. `AsyncFromSyncIteratorContinuation` completed too (promise-unwrapped values, close-on-rejection) — the wrapper's promise now resolves `{ value, done }` with the unwrapped value. **AsyncGeneratorPrototype is 48/48 and the whole async-generator language cluster is closed** (+115 async fixtures overall, zero regressions). The async **`using`/`await using` end-of-body disposal cluster closed too** (11 fixtures): an async function/generator body's resources are disposed at completion in reverse registration order (spec 9.4.3), awaiting async-dispose hints through the job queue and folding a throwing disposal into the completion (nested `SuppressedError`s, delivered to the catch); scopes inside resumable bodies dispose the same way (`UsingInit`/`SetFunctionName` steps, async-aware `LeaveBlock`, try-block envs disposed at `CatchBind`), and a statement containing a `break`/`continue` is compiled so exits cross the compiled-block boundary. **`AsyncDisposableStack` is 104/104** (the `disposeAsync` driver now runs the stack in reverse, folds rejected async-dispose promises as throwing disposals, awaits once for null/undefined-only stacks per 27.4.1.3, and rejects (not throws) on `RequireInternalSlot` failures; `use(null|undefined)` registers the no-method async resource). **Promise is green** (5 fixtures): a capability's resolve/reject now share one `[[AlreadyResolved]]` flag (resolve-then-throw is a no-op), and the harness `Test262Error` has a real prototype so `instanceof` works (the `Promise.any` poisoned-iterable rejections). The final 39 failures then closed with the async-test engine work: `Array.fromAsync` rewritten to spec 23.1.2.4.1 (0-length array-likes skip the loop, elements are awaited before mapping, the result `length` is set on completion — a read-only length or throwing setter rejects — mapper rejections close the iterator, unmapped iterator values are defined without awaiting, and sloppy async-function/generator/async-generator `this` binding boxes primitives); `for await` closes its iterator on `break` (new `AsyncForOfClose` VM step) and object-rest-to-property heads compile their member reference; AsyncFromSyncIterator close-on-rejection uses throw-completion semantics (the original error wins over a non-object `return` result) and `AsyncIterator.prototype[Symbol.asyncDispose]` rejects when the `return` getter throws; `Atomics.waitAsync` registers with the wait registry so `notify` resolves the promise with "ok" (NaN timeouts and an omitted `notify` count are +∞); dynamic import of a fulfilled member of an errored top-level-await cycle rejects with the cycle root's recorded error (cycle-root / async-parent tracking with deferred bodies); and `for (async of …)` is rejected per the `[lookahead ∉ { let, async of }]` early error (escaped `async` and `async of => …` stay valid). **The sweep is now 0 fail across all three areas: 42,042 pass / 0 fail / 0 crash / 0 hang of 48,622 fixtures** |

**Remaining skips — 6,029, out-of-scope proposals or blocked on engine work:**
| Reason | Count | Note |
|---|---|---|
| Temporal | 4,611 | stage-3 proposal, enormous |
| `regExpUtils` | 586 | ~~`v`-flag unicodeSets~~ ✅ **Done** — full binary-property/POS tables, `\q{}`/class-set ranges, property aliases. 586/586 pass |
| import-defer | 252 | ~~stage-3~~ ✅ **Done** — `import.defer()` implemented and un-skipped (114/114) |
| source-phase-imports | 251 | ~~stage-3~~ ✅ **Done** — `import.source()` implemented and un-skipped (243 language + 8 built-ins) |
| ~~`atomicsHelper` / CanBlockIsTrue~~ | ~~119~~ | ✅ **Done** — `$262.agent` host API on real worker threads; `Atomics.wait` *"ok"* notify semantics; cross-thread `waitAsync` resolution. All 119 pass |
| await-dictionary | 89 | stage-3 |
| ShadowRealm | 64 | stage-3 |
| TCO (`tcoHelper`) | 34 | **Hard** — proper tail calls; even V8/JSC skip these |
| `temporalHelpers` | 12 | Temporal helpers |
| import-text / import-bytes | 11 | ~~stage-3~~ ✅ **Done** — implemented and un-skipped |

The story the numbers tell: 5,302 of the 6,029 skips (88%) were out-of-scope proposals (Temporal, `import.source()`, `import.defer()`, await-dictionary, ShadowRealm, import-text, import-bytes); the rest were genuine engine gaps — the regexp `v`-flag (586), worker agents for Atomics (119), TCO (34) — plus a dozen Temporal-helper fixtures. Every gap but TCO and the out-of-scope proposals has since closed; the remaining 4,810 skips are Temporal (4,611), await-dictionary (89), ShadowRealm (64), `tcoHelper` (34), and `temporalHelpers` (12).

## The import-defer / source-phase clusters, done (language at 100% of runnable)

The last skips in the language area were the source-phase-imports
(`import.source()`), import-defer (`import.defer()`), import-bytes, and
import-text proposal fixtures. All four clusters are now implemented and
un-skipped — the final language sweep is **23,690 pass / 0 fail / 0 crash /
0 hang / 34 skip (TCO only)** of 23,724 fixtures, and built-ins is 18,331 /
0 / 0 / 0 / 5,481.

The import-defer cluster (114 fixtures) closed last:
- `DeferredModule.then` aggregates the async transitive dependencies'
  evaluation promises with `PerformPromiseThen` (never `Promise.prototype
  .then` — the patched-method fixture asserts it is not called).
- Deferred namespaces trigger synchronous evaluation on export access with
  the full import-defer MOP: `[[Get]]`/`[[HasProperty]]`/`[[GetOwnProperty]]`/
  `[[DefineOwnProperty]]`/`[[OwnPropertyKeys]]` fire for non-symbol-like
  keys (the `in` operator now walks the prototype chain dispatching the
  trigger, and class-field defines / super-writes dispatch on the receiver
  descriptor read), while `[[Set]]` (any key), `[[IsExtensible]]`,
  `[[PreventExtensions]]`, and prototype ops never do. A same-value `null`
  `[[SetPrototypeOf]]` returns true (SetImmutablePrototype).
- An errored deferred module throws its recorded evaluation error on every
  export access (EvaluateSync), and a sync evaluation that rejects throws
  the result — `err1 === err2` across repeated accesses.
- `GatherAsynchronousTransitiveDependencies` and `ReadyForSyncExecution`
  are `IsModuleSCCEvaluated`-aware: a member of a cycle whose root is still
  EVALUATING-ASYNC is not treated as evaluated, so a deferred import of a
  module in an in-flight async cycle waits for the whole SCC (the
  async-cycle-dependency-of-deferred-module fixture).
- `%AbstractModuleSource%.prototype[@@toStringTag]` getter added (returns
  the `[[ModuleSourceClassName]]`; `undefined` for non-slot receivers), and
  `Reflect.getOwnPropertyDescriptor` reads the live namespace binding.

**Final state — all three areas 100% of runnable: 43,812 pass / 0 fail / 0
crash / 0 hang of 48,622 fixtures (4,810 skips: Temporal 4,611,
await-dictionary 89, ShadowRealm 64, `tcoHelper` 34, `temporalHelpers` 12).**

## The atomics cluster, done (119/119)

The `atomicsHelper` (112) and `CanBlockIsTrue` (7) fixtures closed with
the `$262.agent` host API (`crates/test262/src/lib.rs`):

- **`$262.agent`** — `start` spawns a worker OS thread running a fresh
  agent (`can_block = true`) with its own `$262.agent`
  (`receiveBroadcast`/`report`/`sleep`/`leaving`/`monotonicNow`);
  `broadcast(sab, id)` delivers the shared byte block to every worker and
  blocks until all have received it; `getReport` drains a shared report
  queue (a dead worker's error surfaces instead of hanging the fixture's
  polling loop). The hub (`AgentHub`) is Rust-side (mutex + condvar), not
  a control buffer, because the workers are real threads.
- **`Atomics.wait` returns *"ok"* on a notify** — the entry value check
  alone decides *"not-equal"*; a woken waiter never re-checks the value
  (the old re-check loop turned the standard notify-without-value-change
  pattern into "timed-out").
- **Cross-thread `waitAsync` notify** — a notify from another thread can
  only mark the event's status; the owning agent resolves it via its
  timeout job or `service_wait_async` (the worker's job loop).
- **Real `setTimeout` in the harness** — the atomicsHelper busy-microtask
  fallback starved the `waitAsync` timeout jobs; a timer-backed global
  `setTimeout` (plus a drain-until-`$DONE` loop for async atomics
  fixtures) keeps both the 1 ms timeouts and the 1 s report polls firing.
- **Host builtins link `%Function.prototype%`** (`create_host_builtin`,
  CreateBuiltinFunction) — the atomicsHelper binds `$262.agent.getReport`.
- **SharedBuffer resize UB fixed** (pre-existing, surfaced by the
  `workers` feature): the resize copied `old.len()` bytes into a
  possibly-smaller block (heap overflow on shrink) and views kept stale
  blocks; resizable/growable buffers now pre-allocate their capacity and
  resize only updates the shared byte length in place.
