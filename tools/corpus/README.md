# tools/corpus — cross-engine workload corpus (prototype)

A small, engine-agnostic benchmark corpus plus the runners to execute it
under slag (jit / `--jitless`) and node (jit / `--jitless`), verify all
four modes produce the same result, and print per-workload + per-family
gap tables. This is the slice-0 prototype of a broader corpus — the
standing "corpus probe" the perf notes keep gating deferred work on.

## Layout

- `workloads/<family>/<name>.js` — the corpus. Each file defines
  `function bench() {...}` and ends with `bench();`, returns a Number,
  and is self-contained (no engine-specific globals). Families bucket the
  mechanism under test (`objects`, `calls`, `arrays`, `strings`,
  `builtins`, `control`).
- `workloads/language/...` — workloads generated from test262 fixtures by
  `amplify.js` (see below); the fixture's own `assert` calls guard
  correctness inside the timed region.
- `run_node.js` — the node-side runner (mirrors the CLI corpus mode's
  protocol: eval once, bind args once, 2 warm calls, 3 timed calls,
  report the min per-call time).
- `bench.js` — the orchestrator: runs all four engine/mode combinations,
  cross-checks results, prints the gap table and family summary.
- `amplify.js` — turns plain test262 fixtures (no `negative`/`includes`/
  `features`/module/async gating) into corpus workloads: the body runs in
  an inner function per iteration so its `var`s cannot collide with the
  wrapper, and the iteration count is calibrated so one `bench()` runs
  ~50ms under node. Only fixtures that run cleanly under both node and
  slag are kept.

## Usage

```
# build the corpus runner into the release CLI (jit feature)
cargo build --release -p cli

# run a single engine/mode (machine-readable lines, see run_node.js/CLI)
target/release/slag.exe --corpus tools/corpus/workloads
target/release/slag.exe --jitless --corpus tools/corpus/workloads
node tools/corpus/run_node.js tools/corpus/workloads
node --jitless tools/corpus/run_node.js tools/corpus/workloads

# or the full four-mode comparison
node tools/corpus/bench.js [--slag <path-to-slag>]
```

`SLAG_BIN` also selects the slag binary. `bench.js` exits non-zero if any
engine/mode run fails, and flags rows whose four results disagree.

## Workload format

```
function bench() {
  // ... work ...
  return <number>;   // deterministic; compared across engines
}
bench();
```

Workloads are timed at steady state with the same protocol as the
micro-suite rows (`--jit-bench`): the definition is evaluated once, the
function warmed, then three timed samples report the min PER-CALL time.
Both runners size their timed samples adaptively: a probe call is timed
first and the sample is batched (repeated `bench()` calls) so every
sample clears a ~20ms floor on EVERY engine — a sub-ms workload is
measured from a ~20ms aggregate, not from a single quantized run. The
corpus runs in one process per engine/mode, so per-file startup noise is
excluded on both sides.

## Caveats (prototype)

- Per-workload iteration counts are still hand-set so one `bench()` runs
  roughly 50-1500ms under slag jit (jitless is slower by 1-10x). The
  runner-side sample floor keeps every measurement stable regardless of
  that spread.
- Node's JIT closed-form-folds the affine loops (e.g. `destructure`,
  `push_pop`), so those per-call times are near-constant in the iteration
  count — their ratios measure "V8 scalar-evolution" as much as the
  member/store machinery. Making those workloads data-dependent is the
  fix if their ratios are used for attribution.
- Amplified test262 fixtures return a constant (`1`): their correctness
  gate is the fixture's own asserts throwing on divergence (the run fails
  loudly), not the returned value.
- Result values must be integers < 2^53 (compared exactly across modes).
