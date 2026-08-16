---
name: slag-modules
description: Load when working on Slag's module system (crates/runtime/src/module.rs), dynamic import, top-level await, import.meta, or the test262 flags:[module] fixtures. Documents the non-obvious traps: dynamic import must load asynchronously (DFS evaluation order), self-import aliasing, parse-tolerant _FIXTURE registration, and frontmatter/sweep conventions.
---

# Slag module system traps

These are the traps that cost real debugging time. They apply to
`crates/runtime/src/module.rs`, the test262 module harness in
`crates/test262/src/lib.rs`, and any fixture sweep that touches
`flags: [module]` fixtures.

## 1. Dynamic import is asynchronous — never evaluate synchronously

`import()` must NOT resolve/link/evaluate its target inline. The spec
(`sec-moduleevaluation` Evaluate step 1) asserts Evaluate never runs
concurrently with another Evaluate; the host completes the dynamic-import
load in a job. In this engine that means:

- Defer the whole resolve → instantiate → evaluate → settle sequence with
  `agent.enqueue_generic_job(...)`, capturing `specifier_text`, the
  capability's `resolve`, and `reject` (all `'static` clones).
- By the time the job runs, the current synchronous execution — the
  ongoing DFS evaluation wave — has finished. A target that is also a
  static dependency of the wave is already `Evaluated`, so the job finds
  it settled and resolves the promise with its namespace.

Guard: `test262/test/language/module-code/verify-dfs.js`. The entry
statically imports `a` then `b`; `a`'s body calls
`check(import('./verify-dfs-b_FIXTURE.js'))` BEFORE recording `'A'`. A
synchronous load evaluates `b` first and the order comes out `[B, A]`,
failing `assert.sameValue(evaluated.order[0], 'A')`. Do not "optimize"
this path back to synchronous — the failure is silent ordering, not a
crash.

## 2. Evaluation is one DFS wave, synchronous until top-level await

`module_evaluation` visits the module's static imports in declaration
order, recursing depth-first, and runs each body when its dependencies
are done. The wave claims every statically-reachable module. Cycle
members already on `agent.module_eval_stack` are skipped (the body runs
once, on the first member); `cycle_root` / `async_parents` /
`pending_async` carry error and fulfillment propagation between cycle
members and their waiters. `ModuleStatus::Evaluating` across a job
boundary is a bug — synchronous evaluation never spans jobs.

## 3. Self-imports must alias the entry module record

A fixture that imports itself (`import './<name>.js'`) or closes a cycle
through the entry must resolve to the SAME `SourceTextModule` handle,
not a second parse of the same source. The harness aliases the entry
handle under its own specifier in `realm.loaded_modules` after
registration; keep that alias in place when changing the module path.

## 4. `_FIXTURE` sibling registration is parse-tolerant

test262 dynamic-import fixtures reference `*_FIXTURE.js` /
`*_FIXTURE.json` siblings in the same directory. The harness registers
EVERY sibling, but a sibling whose source fails to parse is registered
anyway — its SyntaxError surfaces when the importing module resolves it.
Intentionally-invalid negative-test siblings must not poison
registration; `phase: resolution` negative fixtures depend on this
surface-the-error-later behavior.

## 5. Module fixtures are always strict and run once

The harness `modes()` returns only `Sloppy` for `flags: [module]`
fixtures — there is no sloppy/strict split for modules. `run_one` routes
module-flagged fixtures to `run_one_module` before any wrapping.

## 6. Frontmatter parsing: parse lists, never match prefixes

Flag order varies (`flags: [module, async]` vs `flags: [async, module]`).
Always split the `[...]` list and check membership. `grep "flags: \[module"`
silently misses reordered flags (~200 module fixtures). The Rust harness
splits frontmatter on both `\n` and `\r` (CR-only fixtures exist, e.g.
the String toString line-terminator tests) — a JS/Python reimplementation
must too.

## 7. Sweep tool conventions

- `test262-sweep --list FILE` paths are AREA-ROOT-RELATIVE:
  `module-code/x.js`, never `language/module-code/x.js`.
- `--filter GLOB`'s `*` matches across `/`.
- `*_FIXTURE.js` files are never fixtures (both `collect_js_files` and
  the sweep skip them).
- Keep the `run_fixture` skip taxonomy (stage-3 proposals: Temporal,
  import-defer, import-bytes, import-text, source-phase-imports,
  await-dictionary, ShadowRealm; host-dependent: CanBlockIsTrue;
  unsupported `includes:`) in sync with `tools/skip_tally.js` — the
  tally tool mirrors it exactly and is dogfooded with the engine.

## 8. Negative phases map to module lifecycle stages

`phase: parse|early` → `parser::parse_module` errors. `phase: resolution`
→ link-time errors, checked against `module_declaration_instantiation`.
`phase: runtime` → the evaluation's rejection (or a synchronous
evaluation error carrying the thrown value). A parse that succeeds when a
parse-phase negative was expected, or a module that completes when a
runtime-phase negative was expected, is a failure.

## Validation loop

Single fixture: point the `debug_module_fixture` test in
`crates/test262/src/lib.rs` at the fixture and run
`cargo test -p test262 --lib debug_module_fixture -- --nocapture`.
Full cluster: regenerate the module fixture list with a frontmatter-aware
scan (the engine's `list_modules.js` or a Python walk that parses each
frontmatter's `flags:` list), then
`target/release/sweep.exe language --list <list> --jobs 8 --timeout 120
--recheck-timeout 120 --json`. Always run `cargo clippy --workspace
--all-targets -- -D warnings` after changes.
