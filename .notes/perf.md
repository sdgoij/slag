# Performance

The standing record for speed work: where the engine is now, what it holds every
change to, and the one plan that carries the rest. This file was restarted
2026-11-14 when the register/materialization arc closed at parity; the history
before it, the dated journal, the failed-experiment register, and the per-arc
records, is at `.notes/archive/perf/` (indexed in its `README.md`).

## Where we are (2026-11-14)

Corpus, 81 workloads, `node tools/corpus/bench.js` medians of three runs on one
machine, parity exact across all four engine/mode combinations:

| Family | n | JIT gap | Interpreter gap |
|---|---|---|---|
| arrays | 6 | 9.60x | 3.21x |
| builtins | 7 | 21.00x | 14.72x |
| calls | 6 | 8.84x | 3.04x |
| control | 5 | 12.45x | 3.21x |
| globals | 4 | 2.19x | 0.74x |
| language | 3 | 5.14x | 4.56x |
| objects | 7 | 8.36x | 1.86x |
| opcost | 38 | 11.60x | 3.21x |
| strings | 5 | 9.48x | 4.15x |
| **All** | **81** | **10.75x** | **4.12x** |

`README.md` ("Against V8") carries the per-workload sample rows and the caveats
that make these numbers readable: `builtins` is the noisiest family and its
ratio is set by the two async rows against a sub-millisecond V8 baseline, and
the `globals` family is a controlled experiment rather than a workload. The
JIT-against-interpreter tables are there as well.

Two statistics that are not interchangeable: the corpus's own `mean-jitGap`
(above) is an unweighted mean of per-row ratios and is dominated by
sub-millisecond V8 rows; a total-milliseconds ratio over the same run is
roughly a quarter of it. Quote one or the other, never a mix.

## 2026-11-15: two register-lane refusals closed

Three cuts. The first two are refusals that had been silently costing whole rows; the third is a probe removed from the closure path.

- **The loop counter's entry constant** (`pass/loop_counter.rs`). The register-body lane emits `IntAdd`/`IntSub` for a counter its int plan proved, but the entry constant (often shared, as in `s = 0`) was still a `Number`, so `normalize_param_types` typed the loop parameter `Number` and `opt_lower` refused the body at `opt:int-operand`. Six rows ran the per-step tier on this alone.
- **The register body's accumulator named read** (`opt/lift/mod.rs`, `LeafOp::GetMemberName`), the other half of the `RunRegBody` blanket refusal, which also blocked six rows. Refused leaf ops are now named individually (`leaf_op_name`); the only register-body blocker left in the corpus is one row's `leaf:BinRemConst`.

Same-session, interleaved, min-of-15 isolated: `objects/warm_store` 41.3 to 24.1 ms and `objects/compound_assign` 49.9 to 30.9 (both bodies now lower through the tier), `opcost/prim_prop` 1.73 to 1.16, `objects/many_objects_read` 23.0 to 18.7. Corpus total, whole-corpus A/B over five alternating rounds against the pre-cut tree: **2623.5 to 2534.4 ms (-3.4%)**, 81/81 identical results.

- **The per-iteration chain** (`function.rs`). A closure creation looked its own fresh record up again after registering it, only to install the creation site's per-iteration chain. Carrying the chain into `register_function` removes that probe: 3347.7 to 2501.0 TSC cycles per closure on a 3-captured-closures loop, and -5.7% on that loop (min-of-15, one aligned binary pair). It must be installed after `shared_compiled_body`, because the compile reads it and the compiled body is shared per site.

Two things this arc re-confirmed about measuring here. A whole-corpus single pass per side is not an instrument: rows whose generated IR was *identical* moved by 30-40% between the two binaries, so the per-row isolated, many-round protocol is what decides a row. And `JIT_DUMP_IR` diffs are the way to establish which rows a tier change can even touch: `python` diffing the dump over all 81 rows (heap addresses stripped) named exactly six rows for each cut, which is what made the layout swings on the other 75 identifiable as noise.

## What the current design has taken as far as it goes

The register and materialization work is exhausted, and it is exhausted at
parity: the lifts that admit more bodies to the optimizing tier, the
per-iteration environment family, `is()`, `!==`, measured 1.002–1.020x against
`SLAG_OPT=0`. That is coverage, not speed, and it is worth having, but it is not
a lever.

The remaining gap is structural. Function-object creation costs ~340 ns per
closure where node eliminates a non-escaping closure outright (0.6 ns per
iteration), and the two `language` fixtures, which create closures per
iteration inside a `for (let x in obj)` head, are ~45% of the corpus's total
gap. No amount of register traffic reaches that.

## What is next

`.notes/register-tier-plan.md` is the plan of record, and its §5 ordering is
not the phase list read top to bottom: **A0**, function-object creation itself,
the ~340 ns term, gates the corpus total, because the two `language` fixtures
are ~45% of the gap and the closure path is two thirds of the larger one. A1/A1b
(coverage) is a precondition on those rows but is not what has to move, **D5**
(the callback lane) must precede **E** (generalized escape analysis) because
escape analysis has nothing to work on until a call boundary disappears,
**F1/F2** (the effect oracle, then GVN/loads) follow, and **B** (the typed value)
is off the critical path, the §1.3 census says the materialization residue is
small. C2 (a stack map) is owed by the first cut that keeps a heap reference in
a register. §1 has the census and the ablation, §7 the traps.

## Gates

Every performance change carries its own documentation in the same commit, and
holds all of:

- the corpus differential exact, `mismatches=0` across `jit`, `jitless`, node,
  and node `--jitless`, with identical results under `--gc-stress` and
  `--nursery-stress`;
- test262 `language`, `built-ins`, and `annexB` at 0 fail in both modes;
- workspace tests, `cargo clippy --workspace --all-targets -- -D warnings`, and
  `cargo fmt --all -- --check`.

A refusal is a parity decision, not a gap: the tier declines a step kind rather
than miscompiling it, and `JIT_DUMP_IR` names every refusal. A lift that makes a
row slower by more than 2% is a missing parity port, not a result.

## Measuring

`.agents/skills/slag-perf-measurement` is the protocol and the trap list: same
session, interleaved, min-of-N; never compare across windows or across binaries
built from different trees; check binary freshness before trusting a number.

Instruments: `node tools/corpus/bench.js` (the cross-engine run that prints the
table above), `python tools/corpus/measure.py` (interleaved min-per-row totals),
`ab_lift.py`, `census_probe.py`, `refusal_probe.py` (now including the
`opt-lower bail` gate, which is a different refusal from `opt bail` and hid six
rows) and `ir_diff.py` (which rows' IR two binaries actually differ on, so a
delta on any other row is known to be layout); in the engine, `JIT_DUMP_IR`,
`JIT_DUMP_CLIF`, `JIT_HELPER_STATS`, and `--features step-stats`.
`ENVIRONMENT.md` documents each switch.
