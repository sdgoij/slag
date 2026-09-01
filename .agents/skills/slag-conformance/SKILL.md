---
name: slag-conformance
description: Load when running or triaging Slag's test262 conformance sweeps (language/built-ins/annexB areas), fixing engine gaps exposed by fixtures, or debugging sweep failures/hangs. Covers the sweep workflow, the failure-triage loop, and the non-module traps (proxy has-trap chain walks, async-module hang misdiagnosis, stale release binaries). For module-machinery specifics (import-defer, dynamic import, cycle roots) load slag-modules instead.
---

# Slag test262 conformance triage

Traps and workflow for the conformance sweeps — the release-build sweep
over the vendored test262 submodule. The engine's own test suite is
`cargo test --workspace`; the sweep is the full-corpus measurement.

## The sweep workflow

- Build the release sweep binary FIRST and after every source change:
  `cargo build --release -p test262`. The sweep runs `target/release/sweep.exe`
  — a stale binary silently measures old code. If results look wrong, check
  the binary's timestamp against `crates/**/*.rs` before debugging the engine.
- Full area: `target/release/sweep.exe language --jobs 8 --batch 32
  --timeout 15 --recheck-timeout 15 --json > out.json` (areas:
  `language` | `built-ins` | `annexB` | `all`). The JSON has
  `total/pass/fail/skip/crash/hang` plus `failures` and `hangs` arrays;
  the `failures` array holds fail AND crash entries (the summary `fail`
  and `crash` counts are separate). HARD RULE: never pass a timeout above
  15 seconds — anything that cannot finish within 15 seconds is by
  definition too slow, and a fixture classified as a `hang` under the 15s
  deadline is a real result (re-run it individually only to confirm it is
  genuinely slow, never to reclassify it away).
- Cluster: `--list FILE` where every line is an AREA-ROOT-RELATIVE path
  (`import/import-defer/x.js`, never `language/import/…`). Generate the
  list with a frontmatter-aware walk (Python or `tools/skip_tally.js` style
  parsing) — a `grep "flags: [module]"` prefix match silently misses
  reordered flags, and CR-only frontmatter breaks `\n`-only parsers.
- Single fixture: `printf 'area\trelative/path.js\n' | target/release/sweep.exe
  --worker` (the worker reads `area\trelative` lines on stdin). For a
  module fixture with debug output, point the `debug_module_fixture` test
  in `crates/test262/src/lib.rs` at it and run
  `cargo test -p test262 --lib debug_module_fixture -- --nocapture`.
- The host shell is Windows Git Bash: `find`/`grep` over the tree is slow —
  use Python for fixture walks and list generation.
- Always finish with `cargo clippy --workspace --all-targets -- -D warnings`
  (must be clean) and `cargo test --workspace` (must be green; the only
  non-fixture test in the test262 crate is `debug_module_fixture`, which
  must point at a PASSING fixture).

## The failure-triage loop

1. Rebuild release, sweep the failing cluster, dump the `failures` array
   (`python scratch/dump_failures.py out.json`).
2. Read the failing fixtures' frontmatter + body FIRST — the `info:` block
   often quotes the exact spec steps (the import-defer fixtures carry the
   full MOP pseudocode). The generated `evaluation-triggers/*` fixtures
   encode the trigger matrix as test cases.
3. Fix the root cause, rebuild, re-run the cluster, then the full area
   (a local fix can regress elsewhere — see the proxy-has trap below).
4. A fixture that previously SKIPPED and now runs can expose a pre-existing
   gap, not a regression — check the old sweep's skip count before
   assuming you broke something.

## Traps

### 1. Hand-rolled prototype-chain walks bypass the proxy `has` trap

`key in obj` (BinaryOp::In) with a proxy in the chain must run the proxy's
`has` trap. A runtime-side walk that probes each chain link with
`get_own_property_key` is WRONG: on a proxy that runs the
getOwnPropertyDescriptor trap, not the `has` trap (the
`built-ins/Proxy/has/call-*` fixtures catch this). Delegate to the crux
`has_property_key` at the FIRST `Proxy` or `IntegerIndexed` object in the
chain and let it continue the walk; only probe with `has_own_property_key`
on ordinary objects. The deferred-namespace trigger dispatch
(`ensure_deferred_namespace_evaluation_key`) can ride along the ordinary
links of the same walk.

### 2. A rejected async fixture reports as a hang

For `flags: [module, async]` (and script async fixtures), the harness
reads the `asyncTestPassed` global BEFORE it checks the module's
rejection, so a body that throws before `$DONE()` surfaces as **"async
test never called $DONE"**, not as the error. When a "hang" has no
plausible stall, dump the entry promise's state — it may be REJECTED.
(Promise-level async fixtures report the rejection properly; it is the
module path that loses it.)

### 3. `Reflect.getOwnPropertyDescriptor` must read live namespace bindings

On a module namespace, the crux `get_own_property_key` returns a
placeholder descriptor whose value is `undefined`; the live value comes
from the module machinery (`namespace_live_descriptor` +
`ensure_deferred_namespace_evaluation_key`). Both `Object` and `Reflect`
variants must use it (`exotic-object-behavior.js` asserts
`desc.value === 1` for an exported `foo`).

### 4. Run the whole area after touching shared dispatchers

Changes to `get_property_key` / `put_value` / `BinaryOp::In` /
`find_ecma_accessor` (the runtime's property-access dispatchers) can break
fixtures in a different area than the one you're fixing — the `in`
operator change in the language import-defer work regressed 4
`built-ins/Proxy/has/*` fixtures. After any dispatcher change, sweep all
three areas, not just your cluster.

### 5. Load-dependent fail/crash/hang classification — diff the union

Batch classification wobbles with machine load: a fixture that errors can
report as `fail`, `crash` ("fixture process died" / "batch process died
mid-fixture"), or `hang` depending on whether its batch times out and how
the individual recheck lands. The known decodeURI/decodeURIComponent
batch-death fixtures moved between `fail` and `crash` on the SAME binary
across runs. When A/B-ing two builds, diff the fail+crash UNION of paths
(from the `failures` array), not the raw summary `fail` counts, and run
both sides under similar load. For a clean comparison, rebuild both
worktrees' sweep binaries first (the parent A/B worktree at
`C:/Users/T/Desktop/jsrt-parent` is at `8c2f0cf`).

### 6. `TypedArray/prototype/reduce/callbackfn-arguments-default-accumulator.js` flakes ~1-2%

The Strict-mode run intermittently fails with `Expected SameValue(«43», «41») to be
true` — `results[1][0]` reads iteration 1's `kValue` (43) instead of iteration 0's
callback return (41). Passes in isolation, under `--gc-stress`, and in most full-area
sweeps; reproduces only when the fixture runs in a batch worker process that already ran
the 17 preceding `TypedArray/prototype/reduce/*` fixtures (16 BigInt + `callbackfn-
arguments-custom-accumulator`) — prefix bisection: `lines[:17] + target` reproduces,
shorter prefixes don't. Root cause unpinned (heap-state/timing dependent, consistent
with a stale pointer in the strict-unmapped `arguments` path); documented in
docs/conformance.md Open items. Re-run the fixture before triaging it as a regression.

## Relationship to the other skills

- `slag-modules` — the module machinery: DFS evaluation waves, dynamic
  import, import-defer trigger matrix, cycle roots, module harness. Load it
  for anything `flags: [module]`.
- `slag-bytecode-vm` — the compiled `Step` VM and compiler
  (`crates/runtime/src/ir.rs`): lowering and stack-protocol traps, and the
  bench-noise reality. Load it when a fix touches the VM path.
- `git-commit-messages` — commit message format.
- Keep the skip taxonomy (what `run_fixture` skips) in sync with
  `tools/skip_tally.js`; as of the TCO closure the only skips are
  Temporal, await-dictionary, ShadowRealm, and the unsupported-include
  fixtures (`temporalHelpers` — the `tcoHelper` (34) cluster RUNS: proper
  tail calls pass all 34 `tco-*` fixtures, including the try/catch/finally
  returns). The `atomicsHelper` (112) and
  `CanBlockIsTrue` (7) clusters run: the harness installs the `$262.agent`
  host API (`start`/`broadcast`/`getReport`/`sleep`/`monotonicNow`), spawns
  worker threads with their own agents, and resolves cross-thread
  `waitAsync` notifies on the owning agent (`service_wait_async`). The
  release `sweep.exe` must be rebuilt after touching the harness or
  runtime.
