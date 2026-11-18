---
name: slag-perf-measurement
description: Measure and A/B a Slag interpreter/JIT performance change without fooling yourself. Load before claiming any speedup or regression from wall-clock numbers, comparing two binaries, or reporting a benchmark row. Covers the same-session interleaved control protocol, the same-binary env seam for rows under 5 ms, `ir_diff.py` to bound a change's blast radius, the helper census (`helper_rows.py`), dependency-provenance and native-layout swings (git vs path dep, `-align-all-functions`), representation changes that are free in one tier and paid in the other, the global-loop-bound confound, jitless vs JIT mode checks, the step census (--features step-stats) and how to read its bucket totals, and the traps that produced a false "minus 19 percent".
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
- **An `opcost/*` row's node column can be a folded constant loop.** Those rows
  are one operation in a bare loop over a literal array, so V8's optimizing tier
  proves the operands and closes the loop: `opcost/array_at`
  (`s = (s + small.at(2)) | 0`) runs 100k iterations in **0.0255 ms** on node —
  0.25 ns/iter, i.e. `s = 300000` — against slag's 1.16 ms, a 45x "gap" that no
  per-op change can close and that says nothing about the op. Same for
  `opcost/prim_prop` (18x) and `opcost/num_valueof` (19x). Judge per-op cost
  against ourselves (same-binary seam, or the helper census) and take the node
  comparison from a row node cannot fold — `arrays/index_loop` 4.4x,
  `arrays/for_of_dense` 4.2x, `arrays/push_pop` 14x all reproduce with the row's
  value matching. A row whose ratio looks wildly unlike the corpus total (2.6x)
  is usually node folding it, not an engine defect.
- **A probe you wrote yourself folds the same way.** `scratch/probe/elem_num.js`
  (`strs = [8,8,8,8]`) measured 0.071 ms on node for 100k iterations of a
  load-plus-add — 2 cycles on a 4-cycle dependency chain, which is loop
  elimination. Keep the probe's operands opaque (a parameter, a global read, an
  accumulating index) before drawing a per-iteration conclusion from it.

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
- **Alignment is not enough for a 1-2 ms row.** Even with
  `-C llvm-args=-align-all-functions=5` on both sides, an inlined prototype-cell
  read measured `opcost/array_at` +73%, `arrays/push_pop` +60% and
  `opcost/array_indexof` +34% across binaries, and -2.9%, +0.9%, -1.5% within one
  binary. Three rows nearly paid for a 1.9x win. Below ~5 ms there is no
  cross-binary measurement, aligned or not.

## The same-binary seam (the instrument for small rows)

Gate the change itself behind an env var read once per process
(`SLAG_CHAIN_PROBE`, `SLAG_UNBOX`, `SLAG_GUARD`, `SLAG_INLINE` are the existing
ones; add yours in `ENVIRONMENT.md`), then A/B the two settings of ONE binary:

```
python tools/corpus/seam_ab.py target/release/slag.exe SLAG_CHAIN_PROBE row/a.js row/b.js
```

Both sides are the same executable, so the delta is the pass's marginal effect
with no layout, no provenance and no rebuild in it. This is the only way that
held up on this corpus: the same probe read +73% cross-binary and -2.9% here.
Prefer it whenever the change has a switchable seam; keep the seam afterwards, it
costs one cached env read per compile.

**The seam must be a `OnceLock`, never `std::env::var` at the hot point.** A
temporary `if std::env::var("SLAG_SKIP_X").is_ok()` inside `register_function`
(a per-closure path) made closure creation read 553 ms where the same work
without it reads 239 ms — a 2.3x artefact of the instrument, which then made the
"skip" variant look *slower* than the baseline. `std::env::var` takes the
environment lock and allocates a `String`; at one call per object it dwarfs the
work being attributed. Cache the answer (`static ON: OnceLock<bool>`, as
`leaf_trace_on` and `jit_stats_on` do) or the measurement is meaningless.

## Bound the blast radius before timing anything

A tier change can only move a row whose generated IR differs. Diff it first
(`python tools/corpus/ir_diff.py CONTROL VARIANT`, heap addresses stripped),
because every other row's delta is layout. This named exactly 6 of 81 rows for
two of the cuts in the 2026-11-15 arc; the other 75 swung 30-40% in the same
whole-corpus passes. A change that is lowering-only (same IR, different emitted
code — e.g. splicing a helper call into an inline sequence) has no such filter
and reports `0 of 81 rows`, so fall back to the helper census: the row whose
helper traffic changed is the one the cut reaches. The inline `Array.prototype.pop`
named itself that way (`arrays/push_pop`'s `ArrayPop` 1.44 M -> 0) while
`index_loop` and `hof_methods`, which contain no `pop`, read +5.5% and +5.3%
purely as layout.

## A row measures a lowering, not the runtime's state machine

Every row runs one body at rest for three timed calls in one process. That
cannot falsify a change that adds per-object or per-body state: a site cache, a
deferred property, a call record keyed by body site, a cache eviction policy, a
guard planted per read. The blast radius is real but invisible — `ir_diff`
reports `0 of 81 rows` for a lowering-only change, and for a state change the
rows that move are layout, not the mechanism.

The states a row cannot reach:

- **Cache residency.** `MAX_CACHE_ENTRIES` is 1024 bodies; a row has tens. The
  eviction path, its use-counter decay, and the consult-count reset never run.
- **`in_flight` at a call site.** The timed body is the outermost run, so a
  deopt can rebuild. A body called from compiled code (`agent.jit_depth > 0`)
  can never rebuild: `JitCache::lookup` skips the stale-entry removal, returns
  the same guarded code, and the body deopts again on the next call. One
  deopt per call, forever, is a shape only a nested call can produce.
- **A guard that fails.** A row with a failing guard is a row whose steady
  state is bad, so it gets fixed or dropped rather than measured.
- **State older than the sample.** A frame over a warm buffer, a warm style
  cache and a warm dispatch table is a different program from a loop that
  builds its operands in the loop head.

So: `tools/corpus/workloads/app` is the family for that shape, and a nonzero
`SLAG_JIT_DEOPT_TRACE` count on it is a tier defect, not a property of the row.
The landing gate for a state-machine change is that family plus the real
workload's frame time in the same session, not the corpus total.

```
SLAG_JIT_DEOPT_TRACE=1 target/debug/slag.exe --corpus tools/corpus/workloads/app
```

## Read the counters as counters, not as compiles

`compiled` (`JIT_COMPILED_COUNT`) is incremented in `lookup_info`'s slow arm,
i.e. by every consult that resolved to a code pointer — a cache hit counts, and
so does the consult that follows a deopt. `jit_info` is cleared by exactly three
things: a `run_jit_body` deopt (`deopt_retries` bumps and the pointer clears),
a cache eviction, and a stale-entry rebuild. `evicted` counts the last two.
Consequences that read as something else:

- **`compiled ≈ deopted` is one deopt loop, not 100 compiles a frame.** A body
  that deopts per call re-consults per call; at ~0.4 ms a Cranelift compile,
  300k compiles in one run would be impossible, and that impossibility is the
  tell.
- **`evicted > 0` does not prove the cache overflowed.** A stale rebuild (the
  body's `deopt_retries` moved past the entry's) is recorded as an eviction.
  Compare `evicted` against the number of *independent* bodies, not against
  `compiled`.
- **A deopt in a leaf lane is not the same event.** `run_jit_leaf`'s deopt arm
  counts `deopted` but neither clears `jit_info` nor bumps the retries, so the
  body keeps its (speculative) code and the caches stay warm. A run whose
  `deopted` is high and `compiled` low is deopting in the leaf lane.
- **A deopt storm is not automatically expensive — price it before removing it.**
  The Goats scene deopted 33k times in a 25 s window (~22 a frame) and it cost
  ~0.13% of the frame, because each body's interpreted tail was a few steps.
  Making a nested deopt rebuildable (old code retired until no frame is in
  flight) removed the storm (27 deopts left) and measured **1.4-2.1% slower** on
  the interleaved phase sum: the rebuilt body runs unguarded, and the guard it
  loses was buying tag-free arithmetic on *every* call. A guard's deopts and a
  guard's win are separate measurements, and the second one is the one a corpus
  row reports.

## Two pins on different lineages are not an A/B

Check the topology before attributing anything:
`git merge-base --is-ancestor <control> <variant>`. A sibling branch used as a
"control binary" differs by every commit on both sides, including JIT design
changes that move exactly the counters under discussion. A counter discontinuity
between such pins says nothing about the commits on the variant's side.

## Do not judge on the corpus total

`head-let` alone swings +-20% (about +-90 ms of a 2400 ms total), which swamps
every gain a session lands. Two whole-corpus passes five rounds apart reported
+0.3% and -4.1% for the same binary pair. Total the rows that are quiet instead
(state the exclusion): the 79 rows other than `head-let` and
`evaluation-order` read -5.2% for the same pair, consistent with the per-row
numbers. Quote the row table, not the total.

## Attribute traffic with the helper census

`python tools/corpus/helper_rows.py` runs one row per process with
`JIT_HELPER_STATS` and attributes the histogram (the array itself is
process-wide, so a whole-corpus run sizes a helper but cannot say who pays it).
Use it before designing anything: it turned "head-let should respond to a cheaper
closure path" into "head-let makes ~58 helper calls per iteration", and it sized
the three cuts after it. A helper-count drop is NOT a win by itself — inlining
`ToBooleanSlow` to zero moved the rows only once the inline sequence was as short
as the call it replaced.

**Counts are not comparable across binaries for a row that changed speed.**
`run_corpus` sizes `reps` from a timing floor, so a faster row runs MORE
`bench()` calls in the same process and its helper total can RISE while the
per-read traffic fell (`array_at` +9%, `array_includes` +17% for that reason
alone). Also note the first `bench()` call in a process runs cold and the
warm-up is 2 calls: a per-iteration figure is `total / (bench calls × iterations
per call)`, and the `bench` line does not print the call count. Read a census
for rows whose speed did not move, or normalise per iteration, and never quote
"the total went down" as the mechanism when it went down by a whole multiple of
the row's per-call traffic.

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

## Reading a per-iteration gap in the generated code

Before designing a cut, convert the row to cycles per iteration and compare it
with the dependency chain it must have. `JIT_DUMP_CLIF=1` prints the CLIF and
the disassembly (`function %jit_opt_body` = the optimizing tier); find the loop
by its back edge and read the hot block, not the entry.

- **A software sequence masquerading as one instruction.** `fcvt_to_uint_sat`
  and `fcvt_from_uint` on `i64` are `cvt_float64_to_uint64_sat_seq` /
  `u64_to_f64_seq`: multi-branch expansions because x86-64 has no unsigned
  conversion. The SIGNED pair (`cvttsd2si`/`cvtsi2sd`) is one instruction each and
  is exact inside `[0, 2^63)`, so a bounded index/count wants the signed form.
  Grep the dump for `_seq` before blaming anything else — the canonical-index
  gate had two of them in every element read's address chain, and switching cost
  `arrays/typed_array` -12.6%, `arrays/index_loop` -3.0%.
- **Loop-carried locals in frame slots, not registers.** `arrays/index_loop` runs
  3M iterations at ~4.8 ns (~17 cycles) against node's ~1.1 ns (~4 cycles = one
  `fadd` latency). The CLIF shows why: `i` and `s` are reloaded from and stored
  to the body's frame (`load v0+16` … `fadd` … `store v0+16`) every iteration, so
  the chain is store-to-load forwarding plus the arithmetic, not the arithmetic.
  A row whose cost is ~4-5 cycles above the arithmetic it performs is a
  frame-slot/lowering question (the register tier), not a missing fast path — do
  not go looking for another op to inline.
