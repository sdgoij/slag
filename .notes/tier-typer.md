# The typer: guard-driven type specialization (stage I8)

**Status: plan (2026-10-07).** `optimizing-tier-impl.md` §6 stage I8 ("a coarse
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
| T1 | `Op::GuardType` + its lowering (via the landed deopt resume); no pass emits it yet | behavior-neutral; a hand-built-IR test proves the deopt and the tag-free fast path |
| T2 | a type guard inserted at a **loop preheader** for a loop-invariant value (the static, no-feedback slice: a member/element read whose cell is validated) | the `read`/`obj_prop` rows: guards removed, tag checks removed |
| T3 | feedback-driven guards (the `I3` records say the site is monomorphic-typed) | no deopt thrash on the corpus |
| T4 | the typer consumes the guard to make arithmetic tag-free across the loop | the hoist (G2b) revisited: does the register headroom return? |

T2 is deliberately **feedback-free**: the correctness premise is a *static* one
("this loop-invariant read is validated by a guard"), mirroring the read plan's
simplification — the typer only *chooses* where to guard, the guard proves it.

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
