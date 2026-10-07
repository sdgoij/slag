# The typer: guard-driven type specialization (stage I8)

**Status: T1 landed (2026-10-07).** `optimizing-tier-impl.md` §6 stage I8 ("a coarse
typer to drive guard elision"). Scoped immediately after the read arc, whose
findings make the case concrete.

## Why, and what it unlocks

The tier's arithmetic pays, per op: two tag checks (`band`+`cmp`), the f64 op, a
canonicalize (`band`+`cmp`+`const`+`select`), a `band`, and a `brif` to a slow
block — ~11-13 instructions plus a branch. That is at per-step parity, but it is
**register-tight**, and the pressure is load-bearing for everything downstream:

- The LICM read hoist cost **~1.8x** because the hoisted value could not stay live
  across the loop (`.notes/tier-guarded-read.md` G2b). A tag-free loop has the
  headroom for it.
- A member/element/call result is `Unknown`, so *every* op on it re-checks. These
  are the `read`/`obj_prop`/`prim_prop` rows.

So the typer is not "another pass"; it is the thing that makes the guards, the
hoists and the register allocator pay off.

## The one piece of machinery it needs is already landed

A type specialization is **speculate → guard → deopt**, and the guard half is
landed: `Op::Check` + `DISPATCH_DEOPT` resume (`.notes/tier-resume-fidelity.md`).
So a type guard is a `Check` on a type predicate, and a wrong guess *exits* rather
than miscomputes. That precedent is the whole reason this is now tractable — the
plan notes it as the machinery V8/SM/JSC can't share, and Slag already has it.

## What a typer must prove

The lattice exists (`ir::Type`: `Never/Undefined/Null/Bool/Int/Number/String/
Object/Hole/Unknown`) but is coarse: `Number` means "a JS number (possibly a heap
number)" and `Int` "a known int32". Sources of a type:

- **Constants** — exact.
- **Frame loads** — the existing `narrow` fixpoint (a slot stored only numeric
  values is `Number`).
- **Arithmetic on typed operands** — `narrow` widens these to `Number`.
- **Comparisons** — `Bool`.
- **Member/element/call results** — `Unknown` today; the gap.
- **The guard** — a new `Op::GuardType { ty }` narrows a value to `ty` for the
  dominated region, or deopts.

## The shape: guard once, then trust

A pass inserts `Op::GuardType` where a value's type is *profitable to assume and
safe to exit on*. Lowering (`opt_lower`):
- for a `Number` guard: `if is_double(v) { continue } else { deopt }` — a
  two-instruction check + the deopt block;
- downstream ops with all-`Number` operands lower to the existing bare path
  (`emit_bare_numeric`), i.e. one f64 op and no tag checks.

The feedback records (`.notes/optimizing-tier-impl.md` §5, the `I3` substrate)
say *what* type a site sees; the typer turns that into a guard. Where the type is
statically provable (`narrow`), no guard is needed at all.

## Sub-slices

| id | content | gate |
|----|---------|------|
| T1 | `Op::GuardType` + its lowering (via the landed deopt resume); no pass emits it yet | behavior-neutral; a hand-built-IR test proves the deopt and the tag-free fast path — **landed** |
| T2 | a type guard inserted at a **loop preheader** for a loop-invariant value (the static, no-feedback slice: a member/element read whose cell is validated) | the `read`/`obj_prop` rows: guards removed, tag checks removed |
| T3 | feedback-driven guards (the `I3` records say the site is monomorphic-typed) | no deopt thrash on the corpus — **not needed; see below** |
| T4 | the typer consumes the guard to make arithmetic tag-free across the loop | the hoist (G2b) revisited: does the register headroom return? — **no; see below** |

T2 is deliberately **feedback-free**: the correctness premise is a *static* one
("this loop-invariant read is validated by a guard"), mirroring the read plan's
simplification — the typer only *chooses* where to guard, the guard proves it.

## T1 landed (2026-10-07)

`Op::GuardType` + its deopt lowering are in (`opt_lower::emit_guard`, shared
with `Op::Check`; the deopt mirrors `args[1..]`, the live operand stack). No
producer emits it, so it is behavior-neutral: the lift, the pass pipeline and
the conformance baseline are unchanged. Only `Type::Number` is checked
(`is_double`); any other guarded type is `Unsupported` (a refusal, never a
silent guess). `dce` keeps it, `cse` skips it, `licm` refuses to hoist it, and
the deopt's `vm.ip` write is null-guarded so the bare-ctx scaffold can run a
guard's deopt. The hand-built-IR test proves the fast path is tag-free (the
guarded arithmetic compiles against an empty helper table; retyping the guard
result `Unknown` makes it refuse) and that a non-Number value returns
`DISPATCH_DEOPT` instead of computing. Gates: `cargo test --workspace` green
(jit 308/0); `cargo clippy --workspace --all-targets -- -D warnings` clean.

## T2 scoping — the placement fork

T2's gate is "the `read`/`obj_prop` rows: guards removed, tag checks removed".
Two placements, and the `tier-guarded-read.md` negative result decides between
them:

- **Preheader (the plan's wording).** Hoist the read's value out of the loop
  and guard it once. This is the read arc's G3 hoist, which measured **~1.8x
  slower** on `read_loop` from register pressure (`tier-guarded-read.md`). Its
  resume step must be the loop entry, which needs `Block::start_step` in the IR
  (the lift knows each block's first step; the IR does not carry it yet). T4's
  tag-free arithmetic is the bet that the register headroom returns — so the
  preheader work and T4's arithmetic are one experiment, not two.
- **In-loop.** Guard the read's result where it is produced (`MemberCellLoad` →
  `GuardType`), typing the `Add`'s operand without hoisting anything. No
  register-pressure change (the value is already live), no `Block::start_step`
  (the guard's resume step is the read's own step, which it carries in `imm`),
  and it removes the arithmetic's tag checks for one guard per read. The cost is
  the per-iteration guard; the win is the tag-freeness.

Recommended first measurement: the **in-loop** placement on `read_loop` — it is
the cheaper experiment (no `start_step`, no hoist) and isolates T4's core
question ("does tag-free arithmetic pay for a guard per read?") from the
register-pressure confound. Build the preheader hoist (with `Block::start_step`)
only if the in-loop guard pays.

## T2 in-loop measurement (2026-10-07) — a positive but non-leaf-only result

The in-loop producer landed behind `SLAG_TYPER` (off by default): the lift emits
`Op::GuardType` on a member/element read's result, typed `Number`, resuming at
the step AFTER the read (the read has already run once, so the interpreter must
not re-run it — a getter would otherwise fire twice; pinned by a getter-counting
test that fails if the resume step is the read's own). The `args[1..]` mirror is
the post-read stack, which is that step's entry stack.

**The leaf lane caps the typer.** `steps_are_leaf` does not exclude the member
steps, so a body whose only "impure" work is `o.x` is leaf-eligible — and
`run_jit_leaf` has no `DISPATCH_DEOPT` handling, so a guard in it cannot resume.
The guard is therefore emitted only for a NON-leaf body (`!body.leaf`), exactly
like the `OPT_PROBE_DEPTH` diagnostic. The `read` row's `read_loop.js` is leaf,
so it is unaffected by the guard; the guard's win needs a non-leaf read loop
(a `LoadIdent` makes it non-leaf).

**The load-bearing prerequisite was a bug.** `narrow` proved a value numeric and
wrote `Inst::ty`, but the lowering reads each operand's `Function::value_type`
(the value table), not `Inst::ty` — and the value table is append-only, so the
pass's type tightening never reached the tag-free path. Fixed (`Function::set_
value_type`; `rewrite` now syncs the value table). This is the piece that makes
the guard pay: without it the guard only skips the read-side tag check while
adding a check of its own (net ~zero); with it the consuming arithmetic sees
both operands `Number` and lowers bare.

Measured (min-of-5/7 interleaved; `scratch/tier-ab/`, `run.js` = tier vs
per-step, `run_typer.js` = the guard on vs off on the tier):

| row | tier/per-step (baseline) | tier/per-step (now) |
|---|---|---|
| `arith_loop` | 0.731 | **0.682** |
| `read_loop` (leaf) | 0.991 | **0.905** |
| `read_loop_nonleaf` | — | 0.909 |
| `licm_loop` / `for_loop` / `lcg_loop` | 0.788 / 0.896 / 0.922 | 0.787 / 0.906 / 0.943 |

The `arith`/`read` gains are the `narrow` value-table fix (it is always on and
changes lowerings for every tier body); they are NOT the guard. The guard's own
incremental effect (`run_typer.js`): `read_loop_nonleaf` **0.944x** (~5.6%),
`read_loop` (leaf) 0.992x (no change, as expected), and the thrash case — a
non-leaf loop reading a STRING property, so the guard fails every activation —
**1.016x**, i.e. neutral (with the guard off the compiled body already calls
`BinarySlow` per iteration; deopting to the interpreter is neither better nor
worse).

**Left off by default.** The guard's win is real but small, and it changes code
gen for every non-leaf member read; T3's decision gate (feedback, or a cheaper
heuristic) should own enabling it, not this slice. The `narrow` fix is
always-on and is the slice's actual payoff.

Gates: `cargo test --workspace` green (jit 312/0); clippy `--workspace
--all-targets -- -D warnings` clean; test262 `language` 23,726/0/0/0 and
`built-ins` 23,820/0/1/0 at baseline both with the typer off AND with
`SLAG_TYPER=1` (the one `built-ins` hang under `SLAG_TYPER=1` was the documented
`copyWithin` wobble — it passes in isolation).

## T3 — no decision gate is needed (measured)

T3's premise was deopt thrash on a read that is not a Number. Measured, the
thrash is neutral: a non-leaf loop reading a STRING property (`read_loop_string`)
measures **1.016x** with the guard on — with the guard off the compiled body
already calls `BinarySlow` per iteration, so deopting to the interpreter is
neither better nor worse. And where the guard is pure overhead — a non-leaf loop
that reads a number property but never uses it arithmetically (`read_bare`,
the read feeds a `FrameStore`) — it measures **0.972x**, i.e. within noise. So
the guard is (mildly) neutral-to-positive everywhere measured, and a gate buys
nothing that can be measured on this machine. The guard stays **off by default**
anyway: the win is small and the corpus A/B is too noisy here to validate a
broad default-on; a future session with a quieter machine can flip it.

## T4 — the hoist still regresses with tag-free arithmetic (negative)

T4's hypothesis was that the tag-free arithmetic (the `narrow` fix) frees enough
registers for the read hoist (G2b) to pay. Refuted: with `Op::MemberCellLoad`
re-added to LICM's hoistable set, `read_loop` goes **0.905 -> 1.701x** and
`read_loop_nonleaf` **0.909 -> 1.657x** (tier/per-step; `arith_loop` unchanged at
0.666). The regression is the same shape as G2b. The reason is structural: the
tag checks are *transient* (not loop-live), while the hoisted cell value *is*
loop-live — so the register pressure is orthogonal to what the typer removes.
`Op::MemberCellLoad` stays out of LICM's hoistable set. The read's win is the
inlining (G2b) plus the now-tag-free arithmetic, not the hoist.

## Gates and traps

- **A wrong type guess must deopt, never miscompute.** Every gate is a full
  test262 sweep plus the micro-bench rows; a guard that elides a check it
  shouldn't is a conformance failure, not a perf one.
- **The resume step for a hoisted guard.** Same trap as the read arc: a guard
  hoisted to the preheader deopts before the loop runs, so its resume step must
  be the loop entry — which needs the IR to carry block step indices
  (`Block::start_step`, the lift knows them). Settle this in T2.
- **`-0`, NaN, and Int vs Number.** `Number` must mean the canonical NaN-boxed
  double; `-0` and integral-valued doubles are still `Number` (not `Int`). The
  existing `canon_double` still applies to any *computed* result.
- **Don't elide the tag check on a value the guard doesn't dominate** — the same
  dominance discipline `verify` already enforces.

## Open decisions

1. **Where guards are placed.** Loop preheaders only (bounded, matches LICM), or
   anywhere a value is re-read (broad). Recommend preheaders first.
2. **`GuardType` vs reusing `Check`.** `Check` carries a boolean condition; a type
   guard is a predicate on a value. A dedicated op is clearer and lets the
   lowering emit the cheapest check per type; recommend a new op built on the
   same deopt block.
3. **How much of this is the typer vs the read plan.** T2 is the read plan's
   guard; the typer generalizes it to any type. Sequence T2 first (it has the
   read rows to measure) and generalize after.
