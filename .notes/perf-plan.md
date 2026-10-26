# Performance plan after the security audit

**Status: PLAN.** Measured 2026-10-25 on the audit's HEAD (`8bc1f77b`) against
the pre-audit `883af3dd`, three interleaved corpus rounds per binary and three
micro-suite rounds per binary. Nothing here is implemented yet. The raw numbers
and the probe definitions live in `.notes/perf.md` (tail).

## Where we are

| harness | pre-audit (`883af3dd`) | now (`8bc1f77b`) | change |
| --- | ---: | ---: | ---: |
| corpus, mean JIT gap | 10.40x | **10.76x** | +3.5% |
| corpus, mean interpreter gap | 3.69x | **3.88x** | +5.1% |
| micro-suite, JIT column |, |, | flat (all 16 rows within 1%) |
| micro-suite, interpreter column |, |, | +8-16% on 11 of 16 rows |

The corpus JIT total drifted +5.3% while V8's own total drifted +3.1% in the
same runs, so roughly +2.2% of it is real; the interpreter loss is not drift.

The README's published `12.22x` predates both this audit and the optimization
work that came before it. Today's number is **10.76x JIT / 3.88x interpreter**,
so the audit cost a few percent of a much larger, already-banked gain.

**Update (2026-10-25, `ee72c0b8`)**: the corpus now reads **10.54x JIT /
3.76x interpreter** (medians of three interleaved rounds), so the audit's JIT
cost has been recovered and the interpreter loss is within 2% of the pre-audit
figure; the recovery came from the leaf-cache and closure work recorded in
`.notes/perf.md` (P1's remaining items were closed as NULLs). `README.md`
carries the refreshed per-family and per-row tables.

By family (corpus, medians of three runs; `+` is a wider gap):

| family | JIT gap | interpreter gap |
| --- | --- | --- |
| arrays | 10.00 -> 10.33 (+3.3%) | 3.62 -> 3.86 (+6.6%) |
| builtins | 7.13 -> 7.41 (+3.9%) | 4.08 -> 4.20 (+2.9%) |
| calls | 10.47 -> 10.92 (+4.3%) | 5.59 -> 5.16 (-7.7%) |
| control | 12.66 -> 12.56 (-0.8%) | 3.72 -> 3.79 (+1.9%) |
| globals | 2.19 -> 2.19 (0) | 0.76 -> 0.85 (+11.8%) |
| language | 6.18 -> 6.50 (+5.2%) | 5.46 -> 5.83 (+6.8%) |
| objects | 9.23 -> 9.22 (-0.1%) | 2.51 -> 2.64 (+5.2%) |
| opcost | 12.24 -> 12.64 (+3.3%) | 3.62 -> 3.91 (+8.0%) |
| strings | 9.26 -> 9.35 (+1.0%) | 4.58 -> 4.63 (+1.1%) |

## What the audit cost, and where

Bisected by building eight checkpoints of the 66 audit commits and timing four
rows on each (single-row probes, ~1% resolution):

| row | pre-audit | `8d0bebf2` | `cbafcd42` | HEAD |
| --- | ---: | ---: | ---: | ---: |
| `opcost/map_for_each` (JIT) | 23.0 | 21.7 | 29.9 | 27.9 |
| `calls/closure_capture` (JIT) | 34.4 | 34.6 | 38.5 | 38.3 |
| `opcost/array_slice` (JIT) | 16.8 | 17.7 | 17.5 | 19.3 |
| `globals/hoisted_local` (control) | 2.5 | 2.5 | 2.5 | 2.5 |

Two commits carry the JIT row damage:

- **`8d0bebf2`**: "move the execution-context stack and function records into
  AgentRoots": `map_for_each` 21.7 -> 25.4 (+17%). The row's 100k iterations make
  400k builtin-to-JS callback calls, and each one now reads the callee's record
  through the roots.
- **`cbafcd42`**: "move the IC caches into AgentRoots": `closure_capture`
  34.6 -> 38.5 (+11%) and `map_for_each` on to 29.9. `closure_capture` makes 1M
  calls to a stable closure from an array element, so it is the call lane's IC
  verdict that got more expensive.

The interpreter damage is spread across the whole AgentRoots refactor rather than
sitting in one commit: `typed-array write` walks 25.99 -> 26.63 (`d08a165b`) ->
27.78 (`8d0bebf2`) -> 28.90 (`2c4e4f9b`, IC value cells) -> 29.69 (HEAD), and
`global read` moves the same way.

**The mechanism is indirection, not work.** Every traced table now lives in
`Agent::roots: Rc<AgentRoots>`, so a hot access is `agent` -> `Rc` pointer ->
field of a large struct -> `RefCell` borrow, where it used to be
`agent` -> inline field. `Agent::ecma_function` additionally goes through
`Ref::filter_map` rather than a plain `borrow`. The lanes that pay it are the
ones that were 4-9 ns per call: `map_for_each` lost 3.7 ms per 100k iterations
(400k callback calls -> ~9 ns each), `closure_capture` 3.9 ms per 1M (~4 ns each).
A 4-9 ns tax on a 4-9 ns lane is a 50-100% tax on that lane, which is why a
"3.5% corpus regression" shows up as +20% on four rows.

## P0: decide what a regression even is

Before optimizing, fix the measurement: the corpus on this host moves ±10% per
row between runs, and one row inflates when `bench_once` mis-sizes its batch
(`opcost/dyn_key_read` read +150% in one run and was flat over 24 isolated runs).
Land:
- the interleaved protocol (one output file per binary, order swapped each round,
  medians over ≥6 rounds), it is what produced the table above;
- a check that discards a row whose *batched* sample disagrees with its own
  minimum by more than 2x, instead of reporting it;
- the pre-audit commit hash as the reference point in `.notes/perf.md`.

## P1, recover the indirection (target: back to 10.40 / 3.69)

### The invariant this phase may not touch

The audit's M-1 fix is structural, and P1 works *inside* it rather than around
it. `AgentRoots` stays exactly what slice 1 made it: a **separate allocation**,
owned through `Rc`, with every field interior-mutable. That separation is the
fix, a live `&mut Agent` says nothing about the roots' memory, which is what
lets a collection run inside a frame that holds a `&mut Agent`, and why
`collect_with` reaches every agent's roots as `&AgentRoots`/`*const AgentRoots`
from the registry (slice 4k).

So, for every step below:

- the roots remain a distinct `Rc`-owned allocation, **inlining them into
  `Agent` is not an option**, whatever it measures (it is the one shape that
  would reopen M-1);
- no `&mut AgentRoots` is ever formed, and no unsafe accessor is added to "buy
  back" an access;
- nothing derived from the roots is stored in TLS, in a global, or in any struct
  that can outlive the run that borrowed it;
- a cached reference is a **local** (`Rc::clone` held for the run) or a `&`
  derived from one, so borrowck, not a comment, enforces its lifetime.

The audit already blessed one instance of this pattern: compiled code captures
its cell bases once per `JitCallContext` (slice 4j, "no compiled addressing
changed, only the pointer's provenance"). P1 extends that pattern to the
interpreter; it does not undo any slice.

**No finding is open.** The report's remediation status says so in as many words
("No finding from this report is outstanding"), and M-1 closed at slice 4k. What
slice 4h leaves behind is a *performance* note, not a defect: it priced the leaf
cache's per-call borrow and recorded the one option it did not take. R2 below
picks that option up as optimization work, under the same invariant, and its
`Cell` table must still hand the collector the entry's `environment` edge (the
entry holds an `Option<EnvRef>`, so the cell is copied out to trace, exactly as
4h's `Copy` verdict arrays do). The audit's own `.rules` line, a traced
container belongs in `AgentRoots`, behind interior mutability, reached through
`&self`, is the rule every step here is checked against.

### Steps

**R1: hold the roots for the run, not per access.** At run entry take
`let roots = Rc::clone(&agent.roots);` and read the hot tables through `&roots`
instead of `agent.roots.<table>` per access. This is safe Rust with no lifetime
argument: the local `Rc` is an independent owner, so it also does not conflict
with the `&mut Agent` the run keeps using. It covers the function records, the
IC cells, the IC caches and the env tables in one move. Do **not** store the
reference in `Vm`: the Vm pool lives inside `AgentRoots`, so a `&AgentRoots` in
a `Vm` would be a self-reference for no gain, pass it (or clone the `Rc` into
the run's locals) instead.

**R2: the leaf cache's borrow.** *Landed 2026-10-25 (`per-iteration`
11.601 -> 10.812 ms, -6.8%; `construct churn` 20.036 -> 19.317, -3.6%;
`opcost/typed_array_for_each` -14.4%, `array_for_each` -11.8%,
`map_for_each` -10.6%. See the perf.md entry for the A/B and the gates.)*
Slice 4h measured the one `RefCell` borrow the
leaf cache pays per call at **+6.3% `per-iteration` and +4.5% `construct churn`**,
and two attempts to buy it back failed (`Cell` conversion 7.0% -> 5.5%; a
`#[cold]` fill split measured *worse* at 11%). The option the slice left on the
table is a `Cell` table of `Copy` leaf records. Two constraints come with it,
both from the audit rather than from this plan:

- the entry holds `environment: Option<EnvRef>`, a traced edge, so the table
  must copy the cell out to hand it to the collector (what 4h's `Copy` verdict
  arrays do), not stop tracing it;
- making the record `Copy` means the `Rc<CompiledBody>` in `LeafEntry` becomes a
  borrowed pointer, which needs a written lifetime argument: the record's `ir`
  owns that body and the table's key is the function id, so the borrow is valid
  for as long as the entry is, but the entry is *evicted*, which is the part to
  argue rather than assume.

This is the interpreter's largest single identified item. **Landed as
`Cell<Option<LeafEntry>>` with a `Copy` record whose body is a borrowed
`*const CompiledBody`: the body is owned for the agent's life by
`AgentRoots::compiled_bodies` (never cleared, the cache `EcmaFunction::ir` is
cloned from), not by the record, so eviction does not end the borrow. The JIT's
cold consult re-derives the record's `Rc` by id in `lookup_info_leaf`.
`global_leaf_cells`/`slot_leaf_cells` stay `RefCell`s: 4h measured that lane at
0%, and the `Copy` record already removed their clone.**

**R3: the call lane's record read.** *Attempted 2026-10-25 and rejected as a
NULL: reading the record under a plain `borrow()` (and merging
`construct_inner`'s two reads into one) is inside the noise floor on every lane
that touches it, `cb_nonleaf` -0.3% min, `fn_call` -0.6% min against a 1%
min-based A/A floor, all `--bench` rows within +/-2%. See the perf.md entry.
The guard is not the cost on a 77-345 ns call; the callback lane's ~345
ns/element is.* `Agent::ecma_function` returns a
`Ref::filter_map` for a one-field read; with R1's `&roots` the table is one hop
away, so read the fields under a plain `borrow()` and drop it before the call
(slice 3b's discipline). Verify the builtin-to-JS callback lane actually hits
this: it is the lane the corpus blames, and `--bench`'s `function calls` row
did not show slice 3b's cost, which suggests that row is leaf-inlined while the
callback lane is not.

**R4: IC cells in the interpreter.** With R1 in place, measure before doing
anything: slice 4j already reports the interpreter cell access at +0.8-16%
(spread includes code layout) against a flat compiled side. If the cells are
still the residual, the fix is to read them through the same `&roots`, not to
change their storage.

Stop rule: if R1+R3 do not bring the four-row probe back within 1% of the
pre-audit numbers, and R2 does not recover its measured 6%, the remaining loss
is not indirection this phase can reach, profile the dispatch instead of
refactoring further, and take the P2 queue.

## P2: the queue that was already there

The audit sat on top of these; they are unchanged and still the engine's own
frontier.

1. **The closure record** (`.notes/closure-cost-plan.md` Stage A): `EcmaFunction`
   is 408 bytes and is copied on insert; the class-only fields behind one box take
   it to ~288. Stage D (the arena-allocated record) was measured twice in its
   map-rooted forms and **rejected both times** (2026-10-20 and 2026-10-25: the
   table then roots every record, promoting each on its first minor, the for-in
   row +70% and +80.6%). Only the record-on-the-object shape remains, unmeasured.
2. **The per-iteration environment**: **DONE** (2026-10-25). The record is now
   an `EnvRecord::Slot` with the head values inline and the names shared per
   loop, so a copy allocates nothing: -9.8% on the for-in row, -11.5% on the
   probe (mins, two binaries; see the `LANDED (2026-10-25)` entries in
   `.notes/perf.md`). An ablation still prices the whole machinery at ~-33% of
   the probe, and what remains is the arena box per iteration, one live record
   per iteration is what per-iteration bindings are.
3. **The capture context**: **DONE** (2026-10-25), same record type: the
   per-call captured bindings are slots with the scope's shared layout, built
   once at certification. -9.9% on the for-in row, -8…-9% on the probes that
   isolate it (mins, two binaries).
4. **The for-in enumeration**: **DONE** (2026-10-25): the cache shares the key
   list, a short chain is snapshotted inline, and `own_string_props` stages one
   vector instead of three. The fresh-object enumeration probes are 9-18% faster
   (see the LANDED entry in `.notes/perf.md`). The null-prototype store is not a
   target (the CORRECTION entry).
5. **Creation-site source texts**: **DONE** (2026-10-25): the resolution moved
   from per closure to per site, `e7` 92.3 -> 78.0 ms and the row -7.1%. `e7` is
   now under the closure plan's 85 ms stop rule, so **Stage D is unblocked** for
   a session that wants the arena-allocated record. `FunctionDeclInit` landed the
   same cell afterwards (-13.9% on a body-level declaration, which instantiates
   per call), so every step-based creation path now resolves per site.
5. **Object-literal named evaluation**: **DONE** (2026-10-25, `ca4ee26c`).
6. **Call lanes**: a member call to a closure created in the same iteration costs
   ~350 ns (the for-in row's asserts are three of them per body). This is the
   parked `.notes/rust-initiated-call-plan.md` take-side work plus the
   fresh-function-id leaf entry. The JIT's certified lane and the interpreter's
   nested lane were then both profiled (`.notes/perf.md`, 2026-10-25): ~52 ns and
   ~235 ns per non-leaf call, ~100 stores of necessary per-activation state with
   no removable block. The call machinery is no longer the lever.
7. **Static inlining for loop bodies**: **DONE** (2026-10-25). The splice now
   takes a callee whose identity the body itself proves: a frame slot written by
   exactly one hoisted `FunctionDeclInit`, whose name is never assigned, and
   whose body compiles to a capture-free certified leaf. No guard, no feedback.
   `calls/direct_leaf.js` 12.29 -> 5.87 ms (-52.2%, per-row min over 5
   interleaved rounds, parity identical); the other 76 rows are inside ±3%.
   `SLAG_INLINE` is now on by default; the feedback-recorded (box-guarded) sites
   live behind `SLAG_INLINE_FEEDBACK` (off) because they remain mixed, a closure
   created per invocation never matches the compile-time box, which is what the
   retracted -93.6% miscompile and the +24% rows in `.notes/perf.md` record. The
   next step for those is code patching or a recompile policy, not a guard.
8. **The two ~15x rows** (2026-10-25, profiled and parked): `objects/warm_store`
   is 5.6 ns per store (the full shape gate + the value-cell refresh; the probe
   merge measured -0.2% and was reverted) plus 1.7 ns per read, against node's
   0.58 ns for the whole iteration, which is elimination, not a cheaper store,
   it needs inlining/escape analysis, or a compiled store cell keyed by object
   id. `control/generator_loop` is ~165 ns per `next()` in the resume path; the
   is ~165 ns per `next()` in the resume path; the
   loop side is worth only ~3% (proved by moving the generator to the script
   scope so the caller lifts). Its one missing lift, `UpdateContextSlot`, carries
   a documented typer read-guard miscompile and is not worth 3%. Numbers in
   `.notes/perf.md`.
9. **The `UpdateContextSlot` guard bug** (new, 2026-10-25): narrowed to
   `guard::run`'s pruning, `SLAG_GUARD=0` and `SLAG_TYPER=0` both pass, every
   other switch still fails, and the IR diff shows pruning removing guards that
   carry `mem2reg`'s promoted-slot materialization (`FrameStore(v36) slot7`).
   Either a materialization a later resume still needs, or `narrow`'s slot proof
   outliving the guard it read. Not fixed; do not land the lift until it is.
10. **A compiled object-keyed store cell** (new, 2026-10-25): built and measured
   at -0.5% on `objects/warm_store` (the store is a latency chain, not an
   instruction count), reverted. Only worth revisiting as part of hoisting the
   store's guard out of a loop, i.e. a store-guard IR op.

## P3: the README is a measurement, not a monument

Refresh `README.md`'s performance section in the same commit as any landing that
moves a family gap by more than 2%, and keep the provenance sentence (commit,
method, run count). The current numbers are in this document's first table.

## Gates (unchanged, plus one)

`cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D
warnings`; `cargo test --workspace`; `tools/build.sh` then the four sweeps
separately at `--timeout 15`; corpus parity 0. **New:** every performance commit
carries an interleaved A/B against the previous commit's binary, and the plan's
reference numbers are re-measured whenever the corpus changes.
