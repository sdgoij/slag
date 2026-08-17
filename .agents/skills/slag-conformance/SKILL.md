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
  --timeout 120 --recheck-timeout 120 --json > out.json` (areas:
  `language` | `built-ins` | `annexB` | `all`). The JSON has
  `total/pass/fail/skip/crash/hang` plus `failures` and `hangs` arrays.
  Use the long deadlines — the default recheck is too short for the O(n²)
  crash-test fixtures and misclassifies them as hangs.
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

## Relationship to the other skills

- `slag-modules` — the module machinery: DFS evaluation waves, dynamic
  import, import-defer trigger matrix, cycle roots, module harness. Load it
  for anything `flags: [module]`.
- `git-commit-messages` — commit message format.
- Keep the skip taxonomy (what `run_fixture` skips) in sync with
  `tools/skip_tally.js`; as of the regExpUtils closure the only skips are
  Temporal, await-dictionary, ShadowRealm, `CanBlockIsTrue`, and the
  unsupported-include fixtures (`tcoHelper`, `atomicsHelper`,
  `temporalHelpers`).
