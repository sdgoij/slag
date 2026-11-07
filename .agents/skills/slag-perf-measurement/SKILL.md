---
name: slag-perf-measurement
description: Measure and A/B a Slag interpreter/JIT performance change without fooling yourself. Load before claiming any speedup or regression from wall-clock numbers, comparing two binaries, or reporting a benchmark row. Covers the same-session interleaved control protocol, dependency-provenance and native-layout swings (git vs path dep, `-align-all-functions`), representation changes that are free in one tier and paid in the other, the global-loop-bound confound, jitless vs JIT mode checks, the step census (--features step-stats) and how to read its bucket totals, and the traps that produced a false "minus 19 percent".
---

# Measuring a Slag performance change

Wall-clock rows on this codebase drift by ~15-20% between runs of the SAME binary,
and numbers recorded at the start of a session do not reproduce later in it. A
variant compared against a remembered number is not a measurement. In one arc a
"−19% interpreter win" turned out to be exactly the control: the stale reference
was 211 ms, five fresh control runs were 166-175 ms, and the variant was 171 ms.

## Protocol

1. **Re-measure the control in the same session, interleaved.** Run control and
   variant alternately (`A B A B A B`), take the min of each, and report both.
   Never compare against a number from an earlier build, an earlier session, or a
   handoff note.
2. **Report the control row next to the variant row.** A speedup claim without a
   same-session control is worthless — including in a progress message.
3. **Keep the workload identical.** Any edit to the benchmark invalidates the
   comparison; keep the control file byte-identical and vary only the binary.
4. **Check that the mode you think you are measuring is the one running.** In JIT
   mode a row that equals its `--jitless` value means the JIT did not engage
   (tier refused, or a lane op the compiled tier does not support). Treat
   "JIT row == jitless row" as a bug signal, not a data point.

## Confounds that have actually bitten

- **A global as the loop bound.** `var N = 3000000; for (var i = 0; i < N; i++)`
  adds a global read per iteration and inflated an entire decomposition table by
  ~6.5x (a 9.7 ns/iter loop read 57.7 ns/iter). Use literals in the loop head so
  the only global access is the one under test.
- **Comparing across a rebuild.** Two feature sets (default vs
  `--features step-stats`) produce two different binaries; never mix their rows.
- **Shell capture losing the last line.** A `for i in 1 2 3; do ./bin ...; done`
  loop can drop the final iteration's output in capture. End the loop with an
  `echo END` sentinel so a missing line is visible.
- **The first sample after linking.** It is frequently the fastest. Take min-of-N
  with N >= 3 and confirm with a repeat before writing the number down.

## Build provenance and native layout

- **Two builds of the same commit can differ by 30%.** A `git` dependency and a
  `path` dependency on identical sources measured 13.7 ms and 10.6 ms for the
  same commit, with identical JIT verdicts and identical generated code. Never
  A/B across provenance: build both sides the same way and say which way. A
  regression that reproduces only in one provenance is a build question, not an
  engine one — the shipped configuration is the one that counts.
- **`-align-all-functions=5` classifies a swing; do not ship it.** It removed a
  slow draw (13.7 -> 10.73 ms) and cost the good build 3% (10.57 -> 10.92 ms).
  If the flag moves the number, the cause is native layout rather than workload;
  if it does not, keep looking. It hides the sensitivity instead of removing it,
  so treat it as a diagnostic and fix the code growth that made the layout
  fragile.
- **A layout swing invalidates a mechanism attribution made across binaries.** A
  "+7.8% frame" measured path-built against a previous pin reversed sign under
  alignment (the newer pin came out 0.2 ms *faster*), so the whole delta was
  layout. Re-run the control with the flag before blaming a commit.

## The step census

For claims about *why* a row is slow (dispatch count, which step families), use the
census rather than guessing from time:

```
cargo build --release -p cli --features step-stats   # produces target/release/slag-steps.exe
./target/release/slag-steps.exe --corpus <DIR> --jitless
```

- `<DIR>` must contain workloads that define `function bench() {...}` and end with
  `bench();`; the runner splits at the last `bench(`.
- One workload per directory, because the bucket histogram is process-wide.
- **`steps` is the timed window; the bucket histogram is not.** `steps` is a delta
  over the timed calls, but `step-bucket` totals include every warm-up and
  calibration call. Divide the buckets by the number of `bench()` calls you
  actually ran (bucket total / reported `steps` gives that count), or you will
  conclude a row executes 56 steps/iteration when it executes 8.
- A fully fused loop shows ~2 dispatches/iteration plus a large `reg-body` count;
  8-23 dispatches with no `reg-body` means the register lane never engaged.

## Before believing a mechanism

If the proposed cause is a cache miss, prove the cache is warm in the failing case
rather than reasoning about it. In the arc above the JIT row for the same workload
ran at 1.0 ns/iteration, which proved the global-value cell and its
`globals_unshadowed` gate were healthy — so the 145 ns interpreter lane could not
have been the cell read, and that hypothesis was wrong.

A lane op added to the interpreter must also be implementable in the compiled
tiers. `emit_leaf_op` in `crates/jit/src/compiler.rs` (baseline) and
`crates/jit/src/opt/lift/mod.rs` (optimizing) both end in
`_ => Err(Unsupported)`, and a lane op that degrades to a runtime helper call
costs tens of ns per use — it turned a 24 ms row into 517 ms. Measure the JIT row
on every interpreter change.

A value-representation knob can be free for the interpreter and expensive for the
compiled tiers. Stopping inline-smi boxing in `Value::Number` took `--jitless`
from 31.5 to 17.7 ns/iter but left the JIT 16% over its baseline, because
`is_number`/`value_as_f64` emit the representation test and a `select` into the
generated code: deleting the *producer* moves no emitted code. Measure both rows,
and read an unchanged `SLAG_OPT=0` row as the signal that the cost sits in the
lowering both tiers share, not in the optimizing tier's lift.
