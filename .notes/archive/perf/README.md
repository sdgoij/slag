# Performance archive (closed 2026-11-14)

The performance record as it stood after the optimizing tier and the register
work landed, moved here when the register/materialization arc closed at parity.
`.notes/perf.md` is the live record from that point on; this directory is the
history behind it.

Read it for provenance, for the failed-experiment register, and for the
measurement behind a decision that is still load-bearing. Do not take current
numbers from here, every ratio in these files is a dated observation, and the
corpus total drifts 15-25% between windows for both engines.

References from `crates/`, `.agents/skills/`, and the live `.notes/` docs name
these files by absolute path (`.notes/archive/perf/<doc>.md`) and cite their
own section numbers; the docs are frozen, so those citations stayed valid when
the directory moved.

## The log

| Note | What it is | Status as recorded |
|---|---|---|
| `perf.md` | The consolidated record, 2026-09-05 → 2026-11-14: the failed-experiment register, the dated benchmark-gate journal of every landing and probe, and the milestone sections (NaN-boxed values, shapes/IC, ropes, the bytecode VM, the GC). 20,500 lines | Superseded by `.notes/perf.md` |
| `perf-program.md` | The 3–5x program, measured against `v0.1.0` | Open when closed; superseded by the optimizing tier |
| `perf-findings.md` | The wider map and the numeric hole, against the SpiderMonkey js shell (2026-10-03) | Superseded |
| `plan-2x.md` | 2x node on the corpus before v1.0, on the 79-row corpus | Superseded by the 81-row numbers |
| `structural-gap.md` | The gap is the object model, not codegen (2026-10-10) | Probe; superseded by the register-tier plan |

## The tier

| Note | What it is | Status as recorded |
|---|---|---|
| `optimizing-tier-plan.md` | The tier's design: specialize and retire, never deopt | Design of record; landed |
| `optimizing-tier-impl.md` | The implementation record, stage by stage | Landed |
| `jit-report.md` | The JIT implementation report: design, fast paths, hardening, validation | Closed |
| `jit-quality-plan.md` | The inline-coverage plan: the helper census and stages G1–G21 | Superseded by `optimizing-tier-plan.md` |
| `jit-compile-threshold.md` | Cut 69, the compile threshold and the `body_has_loop` bypass | Landed |
| `pass-pipeline-fixpoint.md` | Whether one pipeline round suffices | Verified (2026-10-07) |
| `opt-per-op-overhead.md` | The opt tier's per-op overhead against the per-step register lane | Open when closed; the framing slots landed later |
| `opt-arc-regressions.md` | Regressions found re-measuring the int-representation arc | Open when closed |
| `opt-int-representation.md` | `Int` as i32 | Proposed, probe-first; not taken |
| `opt-tier-shape-read.md` | The named read's missing shape fast path | Implemented (2026-10-10) |
| `opt-lift-unreachable-block.md` | `Malformed("unreachable block")` on dead code after a `return` | Fixed (2026-10-11) |
| `tier-typer.md` | Guard-driven type specialization (stage I8) | T1 landed (2026-10-07) |
| `tier-resume-fidelity.md` | The `Op::Check` resume slice | Landed (2026-10-07) |
| `tier-guarded-read.md` | The guarded, hoisted member read | Plan; both hoist attempts measured slower and were reverted |
| `typer-guard-miscompile.md` | The typer read guard miscompiled Deno's header path | Fixed (2026-10-11) |
| `non-leaf-jit.md` | Widening compiled coverage past leaf bodies | Plan of record; closed |
| `leaf-lane-deopt.md` | Extending the deopt resume to the leaf lane | Superseded (2026-10-10) |
| `corpus-regression.md` | The cross-workload slowdown bisected to `efa7bb91` | Open when closed |

## The value, object, and call model

| Note | What it is | Status as recorded |
|---|---|---|
| `call-frame-plan.md` | One frame, not one `Vm` per call | Plan of record for the frame model |
| `closure-cost-plan.md` | Shrinking `EcmaFunction` and its registry | Stages B and A landed; Stage D rejected by measurement |
| `frame-cost-profile.md` | Slag Goat on Slag: where a frame's time actually goes | Reference profile |
| `array-store-plan.md` | The `buildString` shape and dense elements | Resolved (2026-09-01) |
| `dense-store-redesign.md` | Elements kinds and a prototype protector | Stage 1 landed |
| `global-read-cells.md` | The read cell only warmed for object records | Landed for the declarative half (2026-09-16) |
| `v8-shaped-arrays.md` | Smi integers and elements kinds | Scoped, not started |
| `whole-body-register-lane.md` | Whole-body register programs (the `fib` lever) | Not started |
| `rust-initiated-call-plan.md` | Closing the 15x Rust-initiated call gap | Plan only; nothing implemented (2026-10-21) |
| `spidermonkey-study.md` | SpiderMonkey as the primary source for the tier | Reference |

## Why the arc closed here

The register and materialization work reached parity: the lifts that admit more
bodies measured 1.002–1.020x against `SLAG_OPT=0` (coverage, not speed), and
the three alternatives that would have paid were each measured worse or
blocked, the hoist-guard lift 1.36–3.62x slower without region-scoped type
facts, the boxed `ecma_function` record 195 → 326 cycles per closure, and a
lazily-built `SourceInfo` blocked because the FNV hash is the site-cache key.
What is left is allocation and inlining, which is
`.notes/register-tier-plan.md`.
