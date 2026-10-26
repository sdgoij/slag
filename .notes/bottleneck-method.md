# Finding bottlenecks at a useful rate

**Status: PARTLY IMPLEMENTED.** Written 2026-10-26, after the sampling-profiler
detour was reverted. 1.1 (helper names) and 1.2 (the refusal census) landed, with
the sweep script; pillar 4 (PGO) is measured at -19.7% on the corpus and not yet
wired into the build. See the perf journal for the numbers.

## The loop we are in

One hypothesis costs: an ablation switch (or a patch), a release build (3-4 min
with `lto = "thin"` and `codegen-units = 1`), 7 interleaved A/B rounds, a
reading of a 1-4% noise floor, and often a NULL. That is 20-30 minutes per
candidate, and 2-3 candidates a day land something real.

The binding constraint is not measurement, it is **which candidate**. The search
space is every layer of an interpreter, a two-tier JIT, a GC, a property model
and the builtins. An ablation also only prices work you already thought of: it
cannot point at work you did not, which is why the last three sessions spent
their budget on layers that turned out to be 1-5 ns each.

So "a better method" means three separate things, not a better profiler:

1. the engine enumerates its own cost, so the candidate list comes from a sort;
2. one candidate gets cheaper to test, so more fit in a day;
3. at least one lever needs no candidate at all.

## Why not an external profiler

The platform rules out every ready-made one, which is worth recording so it is
not re-litigated: `xperf`, `wpr` and `samply` start an ETW session, which needs
an elevated shell (both return access denied here); `pprof` is
`setitimer`/SIGPROF, POSIX only; `hotpath` measures by wrapping every call in a
timer pair and rewriting `#[inline]` to `#[inline(never)]`, which costs more than
a 155 ns `next()` and changes the code under test. The WSL `perf` route worked
and was reverted as too much scaffolding for the result. A native in-process
sampler remains possible (see the last section) but it is the most expensive
option, not the cheapest.

## Pillar 1: the engine reports its own operation mix

Some of this already exists. `JIT_HELPER_STATS` emits an inline increment per
slow-path helper call, gated at compile time (`HelperCounts`,
`crates/runtime/src/jit.rs`), and `JIT_DUMP_STEPS` names the step a lift
refused. Both are the right shape: exact, and zero cost when off.

What is missing, in order of expected value:

### 1.1 Names, not indices, in the helper histogram

`dump_helper_counts` prints `helper <index> <count>`; the mapping is
`crates/jit/src/helpers.rs`. `Helper::name()` already exists (the error path uses
it). Ten lines to print the name, and it turns the dump into a readable table.
Without it nobody runs the switch twice.

### 1.2 The refusal census (`SLAG_JIT_REPORT`)

The largest class of 10-100x rows is not "this op is slow", it is "this body
never ran compiled": a refused lift, a deopt loop, a threshold not reached, a
cell that never hits. Each of those is decided in one place and printed, per
body, to stderr today, where nobody counts it.

At exit, print one table: `reason -> (bodies, steps, executions)`, where the
reason is the `Unsupported` variant's `&'static str` (already a string), plus the
tier the body actually ran on (baseline, optimizing, interpreter) and its
`jit_evictions`. `JIT_DEOPT_COUNT` already counts deopts.

This is the cheapest way to convert "why is this row 15x" into "this row's body
is refused at `UpdateContextSlot`", and it is exactly the information several
past sessions paid for one row at a time.

### 1.3 Interpreter step histogram (`SLAG_STEP_STATS`)

Count per `Step` discriminant in the dispatch loop, with the same
gated-increment discipline as the helper counters. This answers "which IR op
dominates this row" directly, instead of "which Rust function did the profile
blame". `Step` already has a name for `JIT_DUMP_STEPS` and `--print-bytecode`, so
the table labels itself.

### 1.4 Inline-cache hit rates (`SLAG_IC_STATS`)

Per cell kind (global read/store, member read/store, map cell, chain cell), count
hits, misses and invalidations. A cell that never hits is a fast path that
silently does not exist, and that fact is invisible in wall-clock A/B: it looks
like "this row is 12x" with no explanation. The engine already knows every one of
these transitions.

### 1.5 Allocation sites

`--gc-trace` reports collections, not who allocated. A per-(kind, Rust caller)
count would answer "what allocates in this row" the way the last session had to
infer it from an ablation. Cheaper variant: reuse the helper counter pattern on
`Gc::new_in_place` with a small static site table.

## Pillar 2: a sweep that produces a ranked list

`tools/opcount.js` (or a shell script): run each workload file in **its own
process** (a `--corpus DIR` run merges all rows into one process, so it cannot
attribute counts), parse the `<op> <count>` lines, write a CSV of `row x op`.
Then three sorts:

- rows by engine-side ops per iteration, which is the "worst offenders" list;
- ops by share across all rows, which is where one fix pays broadly;
- for a single row, the row itself is the cost model.

Because counts are deterministic, the CSV is also a **regression table**: the
same binary gives identical numbers, so a diff between two commits is exact and
needs no interleaved rounds. That is the CI piece worth having, and it does not
depend on a stopwatch.

## Pillar 3: make one candidate cheaper

- Deterministic counts let an exploratory build use a fast profile (default
  codegen units, no LTO): layout noise cannot change a count. Keep the careful
  `release` A/B for the claim, not for the search.
- Pin the measurement: `SetProcessAffinityMask` to one core,
  `SetThreadPriority(THREAD_PRIORITY_HIGHEST)`, `timeBeginPeriod(1)`. Cheap, no
  dependency, CLI-only, and it cuts the variance that currently forces 7 rounds.
- Run a row N times inside one process. A 30 ms workload spends a measurable
  share of its process in realm setup, intrinsic installation and compilation,
  which both the corpus floor and every per-process measurement inherit.

## Pillar 4: a lever that needs no hypothesis (PGO)

`-Cprofile-generate=<dir>` on an instrumented build, train on the corpus plus
Goats and one tsc run, `llvm-profdata merge` (the binary ships with the
toolchain), `-Cprofile-use=<file>` on the real build. For a large match-based
dispatch plus a JIT host, branch layout and cold-section splitting is exactly the
shape PGO pays on; published VM numbers are in the 5-15% range. Two builds and a
training run, and it composes with every other change.

The caveat is already written in `Cargo.toml`: layout is performance here (up to
13% between two builds of one source), so this must be A/B'd like any other
change, and the profile becomes a build input that has to be regenerated when the
corpus changes.

Same family, one build each, both knowledge-free: `-Ctarget-cpu=native` (the
build is generic x86-64 today), and a speculative look at whether the JIT could
seed Cranelift branch probabilities from the engine's own feedback.

## Pillar 5, optional: a native sampler

A watchdog thread, `SuspendThread`, `GetThreadContext`, `StackWalk64`, and
dbghelp against the PDB the build already emits: no admin, no WSL, no new
dependency, ~300 lines. It is the only instrument that reports true self time
including inline-Rust cost. Deferred, because pillars 1-4 are cheaper and answer
most questions, and because the profiling attempt showed the effort is in
symbolization and attribution, not in sampling.

## Order of work, and what each is expected to change

1. **Helper names (1.1) and the refusal census (1.2).** Small, and refusal is
   the biggest single class of gap. The sweep then ranks rows by "did not
   compile", and several should turn out to be structural rather than
   micro-optimizations.
2. **The per-row op sweep (1.3, plus `tools/opcount.js`).** Commit the baseline
   table. From then on a bottleneck question is a query against it.
3. **PGO (pillar 4)**, which needs nothing from 1-2. Two builds, one A/B.

Stop rule for the whole proposal: if the sweep does not name at least three rows
whose top op mix or refusal reason was not already known, the counters are not
telling us anything new, and the next step is the native sampler instead.

## Risks

- Counts are not cycles. A count model over-weights cheap ops and under-weights
  one expensive event (a GC pause, a cache miss, an allocation). Counts attribute
  *structure*; the stopwatch and `--gc-trace` attribute *cost*. Cross-check the
  two on any row before acting.
- The sweep is a CI-scale job (77 rows x modes), not an inner-loop tool.
- Every counter is another switch that must stay off in the shipping build. The
  helper counter is the precedent (four instructions, emitted only on request),
  and no counter may be on in a measured A/B.
