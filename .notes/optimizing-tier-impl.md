# Implementing the optimizing tier

`.notes/optimizing-tier-plan.md` is the design and the strategy: why a
TurboFan-shaped tier, what the three engines teach, and the stage order O →
I → E → L → T. This document is the engineering plan: where the code goes,
what the interfaces are, in what order it lands, and what proves each step.
No code lands until its increment is written down here.

## 1. Layering

There is no separate `opt` crate. The crate was built and then rejected: a
new workspace member churns `Cargo.lock` and the workspace for a component
only `jit` consumes. The IR lives **inside the `jit` crate** as
`crates/jit/src/opt/`, kept Cranelift-free by discipline (the module does
not `use cranelift_*`), not by the crate graph.

```
syntax / lexer / parser
        |
     runtime            interpreter, Step compiler, CompiledBody, feedback store
        |
        v
   crates/jit/src/
     opt/            SSA CFG IR, verifier, passes   (Cranelift-free by discipline)
     compiler.rs     per-step lowering              (the current path)
     opt_lower.rs    IR -> Cranelift               (planned, shares the ABI)
```

The IR is the front end: the IR itself, the lift out of the runtime's `Step`
stream, and the passes. It depends on `crux`, `syntax` and `runtime` (for
`Step`, `CompiledBody`, `ScopeInfo`) and deliberately **not** on Cranelift.

The lowering stays in `jit`, beside the current per-step emitter, both
sharing `JitHelpers`, `JitCallContext`, `JIT_SLOW_PATHS` and the `Helper`
enum. Keeping the lowering in `jit` is what stops the ABI forking: there stay
exactly one helper table, one `JitCallContext` layout, one set of dispatch
sentinels.

## 2. Module tree

```
crates/jit/src/opt/
  ir.rs         SSA CFG, type lattice, effect sets
  builder.rs    low-level SSA builder (block-param phis)
  verify.rs     the invariants every pass may assume
  print.rs      textual dump
  lift/mod.rs   pub fn lift(&CompiledBody) -> Result<Function, Unsupported>
  lift/stack.rs operand stack -> SSA values
  lift/cfg.rs   jump targets + fall-through -> blocks/edges
  lift/frame.rs frame slots -> SSA, phi at joins
  pass/mod.rs   the pipeline: fn(&mut Function) in order
  pass/fold.rs  constant folding
  pass/dce.rs   dead-code elimination
  pass/inline.rs
  pass/intrinsic.rs builtin intrinsic splice
  pass/escape.rs
  pass/gvn.rs
  pass/licm.rs
  pass/typer.rs
crates/jit/src/opt_lower.rs    IR -> Cranelift (reuses the helper ABI)
crates/runtime/src/feedback.rs per-site typed records + retire hook
```

The core (`ir`/`builder`/`verify`/`print` plus the lattice/effects and CFG
well-formedness tests) was built as the rejected `crates/opt` increment I0;
it re-lands under `crates/jit/src/opt/`. Nothing else of the I0 tree is
landed.

## 3. The lift (Step -> SSA): the hard part

Preconditions: the body is certified (`CompiledBody::scope.is_some()`). The
lift consumes the same `Step` stream the current JIT consumes, and any step
it cannot express in SSA returns `Err(Unsupported)`; that body keeps the
current per-step path. Speculation is never required to lift a body, so the
lift itself is exact, not speculative.

- **Operand stack.** A `Vec<ValueId>` mirrors `vm.stack`. Each step's
  stack effect is the table `max_stack_usage` (`jit/src/compiler.rs`) already
  encodes: N pops, M pushes. A static assert at every step boundary checks
  the depth equals the interpreter's expected depth, so a lift bug is a
  refusal, not a wrong program.
- **Frame slots.** A per-block map `slot -> ValueId`. A store updates the
  map; a load reads it. At a join, every slot live out of two predecessors
  becomes a **block parameter** and each incoming edge passes its current
  value. This is the phi insertion; there is no separate SSA-construction
  pass because the lift already knows each block's predecessors.
- **CFG.** Block starts are `{0}` ∪ every jump/conditional target ∪ every
  post-branch fall-through. Edges come from the `Step` control variants; a
  conditional's two targets take phi arguments for the values live across.
- **Loops.** A back edge's target receives phis for every live slot and
  stack value; that block is the loop header. The fused `FastLoopHead` /
  `RunRegBody` machinery is *not* lifted in the first cuts — those bodies
  take the current path until the IR can express the fused loop (I2).
- **Source of truth.** Every op the lift emits must mean exactly what the
  interpreter's handler for the same step means. The interpreter stays the
  definition of behavior: it must keep passing every fixture at baseline, so
  a lift divergence is caught by the corpus and the sweeps, not shipped as a
  perf win.

## 4. The lowering (SSA -> Cranelift)

- Blocks map to Cranelift blocks; **block parameters map to Cranelift block
  parameters** and `jump`/`branch` arguments map to block arguments. This
  replaces the current `dispatch_targets` compare chain for lifted bodies;
  the current lowerer keeps it for unlifted ones.
- Helpers are reused unchanged through `Helper` / `JIT_SLOW_PATHS`; a new
  helper is added only when a semantic has no existing one, and then it is
  added in all four mirror sites at once.
- `Op::Check` lowers to a `brif` into a per-body **deopt block** that stores
  `vm.ip` (the step) and the working-stack pointer, then returns
  `DISPATCH_DEOPT`. `run_jit_body` already spills the working region into
  `vm.stack` and returns `Interp`; `run_compiled_body` already resumes the
  interpreter at `vm.ip`. The guard is therefore a resume, not an error.
- The fast-path frame protocol is preserved: the lift addresses the same
  `vm.frame` slots the current lowerer does, so a lifted body and an unlifted
  one are interchangeable at a call boundary.

## 5. Feedback

A per-site record keyed by `(body, step)`: a small typed log of what the
site has seen (shape / element kind / callee identity / operand hint) plus
the generation the entry was validated against. The interpreter's member,
global, call and arithmetic handlers write it; the compiled tier's slow
paths update it on a miss. Invalidation reuses the generations and epochs
that already exist (member/global value cells, `leaf_gen`), so there is no
second invalidation scheme.

The record carries the `ICState` mode and adaptive budget
(`Specialized → Megamorphic → Generic`, `MaxOptimizedStubs = 6`,
`maxFailures = 5 + 40 * stubs`): that **megamorphic valve** is what keeps a
guarded fast path from being slower than the helper it replaced, and it is
also what stops the retire hook thrashing.

**Retirement, not deopt.** A compiled body records the premises it was
compiled under (the cells/generations it validated, the intrinsics it
spliced). Invalidation already fires when a premise dies; the retire hook
consumes it and retires the body, so the next entry recompiles or falls back
to the interpreter. A premise that dies is not a mid-activation event — the
write can only come from a nested call, at a step boundary — so a running
activation completes on the old code. No frame-state, no translation
encoder, no side table; the `DISPATCH_DEOPT` exit in §4 remains a *resume*,
used only where a runtime check is cheaper than coarse invalidation.

## 6. Increments

Each increment is independently landable. `I0` is landed; the rest are
proposals.

The `stage` column maps each increment to `optimizing-tier-plan.md` §6.

| id | stage | content | targets | status |
|----|-------|---------|---------|--------|
| I0 | — | IR core: `ir`/`builder`/`verify`/`print` + tests | none (foundation) | **landed** at `crates/jit/src/opt/` |
| I1 | — | lift (straight-line subset) + identity lowering behind `SLAG_OPT=1` | none (equivalence) | **landed** |
| I2 | — | lift control flow: branches **landed**; simple loops with phis proposed | none (equivalence) | partly landed |
| I3 | O | feedback records + `ICState` valve + retire hook + count probe | none (enabling) | partly landed (I3a–b) |
| I4 | B | builtin intrinsic inlining (scalars first, then array/collection) | `regexp_test`, `array_slice`, `string_indexof` | partly landed, outside the IR |
| I5 | I | trial inlining (caller-specialized records) | `method_call`, `js_call`, `closure_capture`, `hof_methods`, `apply_call` | **opened** (probe-first, below) |
| I6 | E | escape analysis + scalar replacement | `object_keys`, `typed_array_for_each`, `array_alloc`/`object_alloc`, `destructure`, `construct_churn` | proposed |
| I7 | L | GVN + LICM + load elimination | `obj_prop`, `prim_prop`, `element_read/write`, `array_at`, `typed_array` | proposed |
| I8 | T | coarse typer driving guard elision | across I5–I7 | proposed |

Stage B is largely landed already, but not as an increment of this tier: the
`Step::CallIntrinsic` sweep put `Math.*`, `charCodeAt`, array `indexOf`,
`Map.get`/`Set.has`/`Map.set` and `at`/`includes`/`push` into the per-step
path on both engines, retired by a realm `%`-identity check. I4 is therefore
re-scoped to (a) lowering `CallIntrinsic` to an `opt::Op`, so the passes can
hoist and sink a landed intrinsic — without this, L and E treat it as an
opaque `Op::Call` — and (b) the residual rows the by-name, single-identity
substrate cannot reach: `regexp_test`, `array_slice`, and `string_indexof`,
which collides with `Array.prototype.indexOf` on the member name and needs a
multi-identity generalization.

**Ratified 2026-10-04.** Stage C0/C1 (`call-frame-plan.md`) are pulled ahead
of I and E: I and E — the 20x–234x cluster — are gated on the one-frame
substrate, and C1 is itself a ~18x call win. S1/S2 (I1/I2) proceed in
parallel, since their write scope (`crates/jit/src/opt/`) is disjoint from
the frame code. The feedback substrate is the full per-site record, not the
guards-straight-to-Cranelift shortcut of the plan's §9.2. Feedback is per
site; **retirement is per body** — a body aggregates its sites' premises, and
a dead premise retires the whole body.

Every increment owes the same gate: `cargo clippy --workspace --all-targets
-- -D warnings`; `cargo test --workspace`; test262 `language` and
`built-ins`; the wasm suites; corpus parity 0; the jit-loss gate. **A perf
change that costs a fixture is reverted.**

### I5 — trial inlining, in full (opened 2026-10-08; probe-first)

Inline a monomorphic callee's `Step`s at a caller's call site, under a size budget, recursing. Targets (`optimizing-tier-plan.md` §6): `method_call` 6x, `js_call` 3.6x, `closure_capture`, `hof_methods`, `apply_call`. It is the **enabler of I6**: an allocation can be elided only where the code that allocates and the code that consumes it are one body, so escape analysis has nothing to work on until the call boundary is gone.

**The part we get for free.** SM allocates a fresh `ICScript` per call site so a function can be monomorphic *per caller* while polymorphic overall (`optimizing-tier-plan.md` §3.3). Slag's feedback record is keyed `(body, step)` and lives on the caller's `CompiledBody` — the body *is* the caller, so a site's record is already caller-private. Trial inlining needs no `ICScript` analog; the existing key is it (a `we delete` item, plan §3.7).

**I5a — the probe (no transform; land this first, and only this if it says no).**
- Extend `feedback.rs` with a `SiteRecord::Call(CallSite)` variant — the enum's documented seam — where `CallSite` holds the distinct callee identities it has seen (bounded by `MAX_OPTIMIZED_STUBS`, reusing `IcState`/`Observed`). The identity is the callee function's box address (what `runtime::jit::leaf_record_slot` already keys on), `0` for a non-function or unresolved callee.
- The writers: the call-step arms of `run_inner_inner` (start with `Step::Call` and `CallFast`, then the rest), calling `CompiledBody::record_call(ip, id)`, mirroring `record_member_read`. The callee's size is `CompiledBody::steps.len()` of the body the identity resolves to (when it is certified).
- Extend `feedback::summary` to classify call sites — monomorphic-within-budget / polymorphic / over-budget — and count callee-body step sizes.
- **The deciding number:** over the 77-row corpus and `--jit-bench`, the share of *call traffic* (weighted, not just site count) at sites that are monomorphic AND whose callee is a certified body under the budget, against the polymorphic count (the retirements a monomorphic inline would fire). A small share is a recorded negative, and I5 stops at I5a.
- Gate: §6's gate, no perf claim; the probe is off by default (`SLAG_FEEDBACK`).

**I5b — the premise and the retire hook (shared with I4/I6/I7).** An inlined callee's identity is a premise, and it is the first premise that is not a value-cell generation. A body carries the premises it was compiled under; invalidation retires the caller's body (`optimizing-tier-plan.md` §5). This is I3's remaining slice and lands here because inlining is the first transform that needs it.

**I5c — the transform (the simplest shape).** At an `Op::Call` whose site record is `Specialized` and whose callee is a certified body under the budget, lift the callee's `Step`s with the same exact `lift` (no speculation) and splice them at the call: the call's arguments bind to the callee's parameter slots, and the callee's return becomes the call's result. Recurse under the budget.
- **First cut admits only a guard-free callee** (`JitCompiledInfo::deopts` empty) — the L2 invariant (a body whose compiled code can deopt is refused by every machine-code inline lane). The frame invariant (`optimizing-tier-plan.md` §2) is the reason: an inlined body that exits must resume at a step boundary with the operand stack exactly as the interpreter expects, and a resume across an inlined frame is work this cut does not do. Widening to a deoptable callee owes that work and must say so.
- Differential tests: a callee with captures, `this`, `arguments`, a tail call, and a throw.

**I5d — widening.** Recursion beyond one level, then the callee shapes the leaf lane already admits (captures, `this`, `arguments`).

**Traps.** §8's list applies in full; two consequences are I5-specific. Inlining grows a body past `JIT_MAX_COMPILE_STEPS` (128 debug / 1024 release), and an over-cap body silently returns to the interpreter, so measure sizes as inlining lands. And the inlined callee's heap references become SSA values, invisible to the conservative stack scan, so they must be in `jit_roots` across a safepoint. Retire thrash — the design's central risk — is what I5a's polymorphic count measures before I5c ships.

**Open (settle as I5 lands).** The inlined callee's IR source (cache the lifted `Function` per certified body, or re-lift per site); the size budget's number (start from I5a's step-size histogram, then SM/V8's ~30 B "small"); and whether the first cut's premise is the callee identity alone or also the argument count.

## 7. Measurement discipline

Probe before lever (`scalecheck.sh`, `--gc-trace`, the count probes).
Interleaved A/B of two binaries, min-of-3, release only. Record the manifest
the corpus writes; a TSV that outlives its binary is already a known trap.

## 8. Traps

- **`JIT_MAX_COMPILE_STEPS`** (128 debug / 1024 release). Inline expansion
  grows bodies; a body over the cap silently returns to the interpreter.
  Measure body sizes as inlining lands.
- **GC roots in optimized frames.** SSA values are not on the conservative
  stack scan. A live SSA value holding a heap reference, and any object
  escape analysis sinks, must be in a traced region (`jit_roots`) or
  materialized before a safepoint.
- **Resume fidelity (was: deopt fidelity).** `e.stack`, `using` disposal and
  inlined-frame naming have each broken once. Retirement makes resumption the
  common path, so a resume must land at a step boundary with the operand
  stack exactly as the interpreter would have it.
- **The leaf lane.** `run_jit_leaf` / `run_inline_leaf` run an entry
  directly and do not handle `DISPATCH_DEOPT`. A lifted body must not be
  leaf-compiled until the lane handles it.
- **The helper ABI is a four-file mirror.** Reuse it; do not fork a private
  copy for the IR lowering.
- **Sealed-block discipline.** The Cranelift lowerer's block sealing rules
  (`seal_all_blocks`, back targets) apply to lifted bodies too.
- **Single-sourced ops.** The lift, the passes and the lowering all target
  `opt::Op`. A per-tier copy of a semantic is exactly what CacheIR exists to
  prevent.
- **The interpreter is the oracle.** It must stay at baseline fixture counts
  throughout; the JIT is faster, never different.

## 9. Immediate next increment (I1), in full

Ordering note (2026-10-04): C0 (`call-frame-plan.md` §4, "Stage C0 detail")
lands first; I1/S1 below runs in parallel with it, and I3 (the per-site
feedback record) is no longer gated behind Stage B.

- `crates/jit/src/opt/lift/mod.rs`: `pub fn lift(body: &CompiledBody) -> Result<Function, Unsupported>`.
- Subset for I1: `Push`/`Pop`/`Dup`, `LoadLocal`/`StoreLocal`/`InitLocal`,
  `Binary`/`BinaryImm`, unary, `Return`, unconditional jumps, and frame
  slots. Every other step returns `Unsupported`. Control flow that is a
  plain fall-through or an unconditional jump only.
- `crates/jit/src/opt_lower.rs`: lower the lifted IR to Cranelift, reusing
  the helper table and `JitCallContext`. Enabled by `SLAG_OPT=1`; a body is
  lowered only if `lift` fully succeeds.
- Test: for a corpus of straight-line functions, the lifted-and-lowered
  result equals the interpreter's. Gate: §6's list, with the corpus at
  parity 0 and no row regressed.
- No perf claim in I1; it proves the lift/lower round-trip is exact before
  any pass rewrites anything.

**I1 status (2026-10-07): the lift landed as `crates/jit/src/opt/lift/`.**
`lift(&CompiledBody) -> Result<Function, Unsupported>` covers the straight-line
subset — `Push` of a number or boolean, `Pop`, `Dup`,
`LoadLocal`/`StoreLocal`/`InitLocal` (frame slots), `Binary`, `BinaryImm`, the
non-coercing-free unaries (`+`/`-`/`~`/`!`), and `Return` — with
`Unsupported::{Uncertified, TdzSlot, Step, NoReturn, Stack, Invalid}`. It
refuses control flow (I2) and TDZ-checked slots (the `is_uninitialized` check
becomes an explicit op later; a body without one is params/`var`s only, so the
check can never fire and the check-free `FrameLoad`/`FrameStore` are exact).
The produced graph is run through `verify` before it leaves the lift. The
identity lowering (`opt_lower.rs`) and the `SLAG_OPT=1` switch are the
remaining half of I1, and nothing consumes the IR yet, so this slice is
behavior-neutral by construction (10 unit tests in the module).

**I1 status (2026-10-07): the identity lowering landed, so I1 is complete.**
`opt_lower::compile` lowers the lifted IR to Cranelift, reusing `jit_sig`,
`helper_sig`, the helper table and the `JitCallContext` ABI: `Const` -> a
NaN-boxed `iconst`, `FrameLoad`/`FrameStore` -> a load/store at `frame +
slot*8`, the arithmetic/bitwise/comparison ops -> `BinarySlow` with the
`BinaryOp` discriminant, the coercing-free unaries -> `UnarySlow`, and
`Return` -> `return_`. A helper call carries the pending-error ABI (a helper
that errored bails the body with `undefined`) and bumps the leaf-epoch when it
can re-enter. The switch lives on `JitEngine` (`SLAG_OPT`, or
`JitEngine::with_opt`); a body the lift refuses — or the lowerer declines —
falls through to the per-step path, so the tier is strictly opt-in and a
default build is unchanged. `crates/jit/src/compiler.rs` grew
`JitEngine::with_opt` and a shared `assemble` so both lowerings hand the
runtime an identical `Compiled`.

**Gates.** `cargo test -p jit` 276 passed (a new e2e test lowers a real
straight-line function through the IR and matches the interpreter, asserting
the lowering actually ran); `cargo test --workspace` all green. With
`SLAG_OPT=1`: test262 `language` 23,726 / 0, `built-ins` 23,820 / 0 / 1,
`annexB` 1,086 / 0 — the whole `all` area at 0 fail on the optimizing path.
No perf claim (I1 is equivalence); the passes (I3+) are what move rows.

**I2 status (2026-10-07): forward control flow landed (branches and forward
jumps); loops are I2b.** The lift now builds a CFG: a block starts at 0, at
every jump target and after every terminator; `Jump`/
`JumpIfFalse`/`JumpIfTrue` and a fall-through become `Term::Jump`/
`Term::Branch`. Frame slots stay in memory (the IR's `FrameLoad`/`FrameStore`
read/write `Heap::Slots`), so **only the operand stack is SSA** — a join takes
its stack as block parameters and each predecessor passes its stack as edge
arguments (verified by `verify`'s arity/dominance checks). `opt_lower` lowers
the multi-block form: one Cranelift block per IR block, IR parameters to
Cranelift block parameters, `jump`/`brif` with `BlockArg`s, and the branch
condition through `ToBooleanSlow` (its result tested `!= 0`). A back edge is
refused (`Step("loop")`) because its target's parameters need a predecessor
that has not been lifted yet — the fixpoint over join depths is I2b. `Throw`
is refused. Two e2e tests (straight-line and a branch/ternary body) match the
interpreter and assert the tier actually ran; `cargo test -p jit` 278 passed,
workspace green, and with `SLAG_OPT=1` the whole test262 `all` area is at 0
fail. Still no perf claim.

**I2b status (2026-10-07): loops and the completion register landed, so I2 is
complete.** The lift admits back edges (a block start at a jump target that
precedes it is no longer refused) and models the statement-completion steps
(`SetCompletion`/`ResetCompletion` -> `Op::CompletionStore`/`CompletionReset`,
both `World` writes; `FusedStoreLocal` -> store + completion;
`NormalizeCompletion`/`ListBegin`/`ListEnd`/`SaveCompletion`/
`RestoreCompletion` -> no-ops, matching the compiled model). Join block
parameters are no longer sized from an already-lifted predecessor: a
**stack-depth fixpoint** (`stack_depths`) runs over the successor edges first,
so a loop header's parameters are known before the header is emitted, and every
join is **uniform** (a single predecessor passes its stack too). `opt_lower`
grows an entry prologue when anything targets IR block 0 (Cranelift forbids
jumping to the entry block) and lowers the completion ops to stores of
`Vm::completion`/`completion_is_empty` (the `Vm` pointer is loaded from the ctx
inside the helper, not hoisted, so the entry block stays pristine for the I2a
re-switch). A `do`/`while` reduction e2e test matches the interpreter and
asserts the tier ran; `cargo test -p jit` 279 passed, `cargo test --workspace`
green, `cargo clippy --workspace --all-targets -- -D warnings` clean. The
`while`/`for` shape still reaches the interpreter's `FastLoopHead` fusion first,
so the lift's loop path is exercised by `do`/`while` today. Still no perf claim.

**I3a status (2026-10-07): the feedback substrate and the count probe landed.**
The store is **per body** (`.notes/optimizing-tier-impl.md` §10.1, resolved
per-body to match §5's `(body, step)` keying): `crates/runtime/src/feedback.rs`
defines `IcState` (the `Specialized → Megamorphic → Generic` valve),
`MAX_OPTIMIZED_STUBS`/`max_failures`, a bounded `MemberReadSite` (the receiver
maps a site has served, in first-seen order), the `SiteRecord` enum, and the
`Feedback` store (one record per step). `CompiledBody` gains a
`feedback: RefCell<Option<Feedback>>` field, allocated lazily on the first write
(exactly the `jit_info`/`jit_calls` pattern), so a default build allocates
nothing. The interpreter's `Step::GetMemberName` arm is the first writer
(`CompiledBody::record_member_read`), gated on `feedback::enabled()`
(`SLAG_FEEDBACK`) — one cached boolean load on the hot path when off. `writes()`
is the stage-O count probe. Nothing consumes a record yet; the `ICState` valve
and the retire hook, and the compiled tier's slow-path writers, are the next
slice, and the retired-premise read (which cell/generation served a site) lands
with the guard that needs it. Gates: `cargo clippy --workspace --all-targets --
-D warnings` clean; `cargo test --workspace` green (5 new `feedback` tests,
including an end-to-end probe that runs a member-read script with the forced on
and asserts records were written); behavior-neutral by construction when off.

**I3b status (2026-10-07): the valve acts and the probe classifies.**
`MemberReadSite::observe` now returns an `Observed` (`Shapeless`/`Repeat`/
`NewMap`/`Overflow`/`Frozen`): a site whose log has overflowed the stub budget
turns generic and **freezes** — later reads touch no record, the `ICState`
valve's first real action. The probe (`feedback::summary`) counts writes plus two
classification counters: the sites that went from monomorphic to polymorphic
(the plan's "retirements that would fire" — a monomorphic guard built on the
first shape would have to retire) and the sites that overflowed to generic. Two
pieces are now explicit decisions rather than oversights: (1) capturing the
*serving* premise (the member cell's generation, or a global cell's) needs the
handler to receive its site — a signature change to `get_member_name`, so it
lands with the guard that reads it; (2) the compiled-tier writer needs the step
`ip` in the member-read helper ABI (the compiled code keeps `vm.ip` current only
at try-exit and deopt probes, not per step), a four-file-mirror change the plan
warns against, so it lands with the first speculative transform. Gates: `cargo
clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace`
green (6 `feedback` tests, including a classification probe); behavior-neutral
when off.

**Pass pipeline status (2026-10-07): fold + DCE landed (`opt/pass/`).** The tier
now has a transform stage between the lift and the lowering (`pass::run` =
`fold::run` then `dce::run`). `fold` is exact: it rewrites an op only when its
operands are constants of a kind that makes the op a plain IEEE-754 operation
(two numbers for `+`/`-`/`*`/`/` and the ordering comparisons, two numbers or two
booleans for equality, a boolean for `!`, a number for unary `-`), and only when
the operand constant is defined in the same block, so the verifier's dominance
rule is never at risk; a mixed-kind `==`, a string operand, and the bitwise
`%`/`**` ops are left for the typer's narrower folds. `dce` drops an instruction
whose result nothing uses, is effect-free, and is not `Op::Check` (whose
`default_effects` are pure but whose removal would drop a speculation guard). The
pipeline is Cranelift-free and unit-tested on hand-built IR; `JitEngine::compile`
re-verifies after the pipeline and bails to the per-step path on a violation (a
pass bug is a refusal, never a wrong program). The tier is still behind
`SLAG_OPT`, so a default build is unchanged. Gates: `cargo test --workspace`
green (289 jit tests, +10: 5 `fold`, 4 `dce`, and an e2e that proves a fold fires
on a real body via a counter); `cargo clippy --workspace --all-targets --
-D warnings` clean; with `SLAG_OPT=1`, test262 `language` 23,726 / 0 and
`built-ins` 23,820 / 0 / 1 (both at the I1 baseline). No perf claim — the passes
are conservative (an arithmetic op's sound default effects are `World`, so DCE
only removes dead constants today); the narrowing is the typer's job.

**Lift broadening (2026-10-07): member and global reads.** The lift now accepts
the two dominant read steps: `Step::GetMemberName` -> `Op::MemberLoad` and
`Step::LoadGlobal` -> `Op::GlobalLoad`, both carrying the name as `Imm::Atom`,
and `opt_lower` lowers them through the *same* helpers the per-step path uses on
its slow paths (`Helper::GetMemberName`, `Helper::GetGlobal`), so a body meaning
is unchanged at the call boundary and the tier can now compile real
property-reading code (before this it could only lift pure `var` arithmetic).
Both carry an opaque `Effects::call()` (a getter, a proxy trap, or a throwing
receiver can run user code), so no reordering pass may move them.

**The next gate is TDZ.** Any body with a `let`/`const` slot whose runtime `TDZ`
check would fire is still refused whole (`Unsupported::TdzSlot`), which excludes
most real functions from the tier. Lifting it needs a `TdzCheck` op (load the
slot, compare to `UNINITIALIZED_BITS`, call `Helper::TdzError` on the
uninitialized marker), so it is the immediate next slice together with
`GetMemberComputed`; only then does "real code" enter the tier at all. Gates for
the read broadening: `cargo test --workspace` green (5 opt-tier e2e tests, incl.
a property-reading body that asserts the tier lowered it); clippy clean; with
`SLAG_OPT=1` test262 `language` 23,726 / 0 and `built-ins` 23,820 / 0 / 1, both at
baseline.

**TDZ status (2026-10-07): lexical slots lifted.** The whole-body
`TdzSlot` refusal is gone. The lift threads `scope.tdz_store` into `emit_step`
and, for a lexical slot, emits an `Op::TdzCheck` (a `Slots` read) before a
`LoadLocal`/`StoreLocal`/`FusedStoreLocal` — `InitLocal` (the initializing
store) needs none. `opt_lower` lowers a `TdzCheck` by loading the slot,
comparing it to `UNINITIALIZED_BITS`, and on a match calling `Helper::TdzError`
(a new `sig_tdz` = ctx-only, mirroring the per-step `emit_tdz_check`): the
helper sets the pending error and the body bails with `undefined`, which the
runtime surfaces as the `ReferenceError`. With reads and TDZ both lifted, an
ordinary `let`-using, property-reading function now enters the tier. Gates:
`cargo test --workspace` green (292 jit tests; a `let`-body e2e that asserts the
tier lowered a body it previously refused whole); `cargo clippy --workspace
--all-targets -- -D warnings` clean; with `SLAG_OPT=1` test262 `language`
23,726 / 0 and `built-ins` 23,820 / 0 / 1, both at baseline — the TDZ-throwing
fixtures included. Next: `GetMemberComputed`, then the first transform that
consumes the widened IR.

**First transform + a number (2026-10-07): inline numeric arithmetic.** The
lift now also takes `Step::GetMemberComputed` (`o[k]`) into `Op::ElementLoad`,
lowed through `Helper::GetMemberComputed`, completing the read set. `opt_lower`
is no longer helper-only: `+`/`-`/`*`/`/` and the ordering comparisons emit an
inline numeric fast path — when both operands carry the double tag, compute in
f64 registers (bitcast, `fadd`/`fsub`/`fmul`/`fdiv`/`fcmp`, result
canonicalized against the tag region exactly like `Value::Number`) and take **no
helper call and no interpreter round-trip**; otherwise fall through to
`BinarySlow`. This mirrors the per-step lowerer's `emit_binary_known` float path,
so the tier's hot arithmetic costs what a per-step body's does, and the tier's
structural advantage (no per-step dispatch) can show. Measured on an arithmetic
`do`/`while` loop (5M iterations, `tools/corpus` protocol, min-of-5 interleaved,
`SLAG_OPT=0` vs `SLAG_OPT=1` on one binary): **24.8ms per-step -> 20.5ms IR,
~1.21x** — confirmed the body ran as `jit_opt_body` (the IR path, 0 bails), not
noise. A single-run corpus A/B over all 77 workloads showed **0 result
mismatches** with the tier on. Gates: `cargo test --workspace` green (293 jit
tests); `cargo clippy --workspace --all-targets -- -D warnings` clean; with
`SLAG_OPT=1` test262 `language` 23,726 / 0 and `built-ins` 23,820 / 0 / 1, both
at baseline. Still behind `SLAG_OPT`; the number is the first time the tier beats
the per-step path, and it is the base the next transforms build on.

**CSE + numeric narrowing (2026-10-07): the first algorithmic win.**
`pass/narrow.rs` computes, by greatest fixpoint, which frame slots and values
are Numbers (a slot is numeric when it is stored at least once and every store
is numeric; a value is numeric when it is a numeric constant, a load of a
numeric slot, or arithmetic on numeric operands) and widens the provably-numeric
arithmetic ops from their sound `call()` default to `pure()`. That is the
unlock: with arithmetic no longer a `World` write, `pass/cse.rs` can apply. It
CSEs block-locally — a pure op or a `FrameLoad` whose identity (op, immediate,
resolved args) was seen earlier with no intervening `Slots` write — rewriting the
second's uses to the first; `pass/dce.rs` now also drops the leftover unused
`FrameLoad`s. Measured on a loop whose body computes `a * b` three times over
invariant numeric locals (corpus protocol, min-of-5 interleaved, `SLAG_OPT`
0 vs 1): **51.3ms per-step -> 31.3ms IR, ~1.64x**, and a dump of the emitted code
shows the three `fmul`s collapsed to one (2 total across the two test bodies).
The plain arithmetic loop stays ~1.21x, so the extra win is the CSE, not the
structural advantage. Gates: `cargo test --workspace` green (297 jit tests; 13
pass-unit tests, incl. the narrowing fixpoint and the CSE invalidation); `cargo
clippy --workspace --all-targets -- -D warnings` clean; with `SLAG_OPT=1` test262
`language` 23,726 / 0 and `built-ins` 23,820 / 0 / 1, both at baseline.

**Bitwise inline, and the regression it removed (2026-10-07).** The tier's
integer path for `&`/`|`/`^`/`<<`/`>>`/`>>>` is now inline: both operands
Number-tagged and inside `ToInt32`'s range (`|x| < 2^63`, mirroring the per-step
`emit_int_binary` + `trunc_i32`) -> truncate to i32, apply the op, convert back
and canonicalize; else `BinarySlow`. Before this, every bitwise op was a helper
call, so the tier *lost* on bitwise-heavy code: measured on a linear-congruential
loop (5M iters, min-of-5 interleaved) the tier was **96.4ms against 47.4ms
per-step — ~2.0x slower**; after, it is **47.8ms against 48.4ms — parity**, a
~2.0x improvement for the tier. That is not a new win but the removal of a hard
regression, which the tier needs before it can be enabled at all (its reads have
the same gap, below). Gates: `cargo test --workspace` green (298 jit tests, one
new bitwise-loop e2e); `cargo clippy --workspace --all-targets -- -D warnings`
clean; with `SLAG_OPT=1` test262 `language` 23,726 / 0 and `built-ins`
23,820 / 0 / 1, both at baseline.

**Fused-test steps + unguarded numeric (2026-10-07).** The lift now accepts the
fused loop/strict-equality test family as terminators — `JumpIf{Lt,Le,Gt,Ge}Imm`,
the `…GlobalImm` siblings, `JumpIfRelLimit` and `JumpIf{Eq,Neq}Imm` — emitting the
comparison plus a `Branch` (exactly `jump_if_rel_imm`/`jump_if_rel_limit`/
`jump_if_rel_global`/`jump_if_strict_eq_imm`, TDZ check included). That alone lets
a `for` loop with a literal bound and a non-`++` update (the *non-fused* path)
enter the tier, where before the test step bailed it. In the same slice, the
`narrow` pass now also marks a load of a numeric slot as `Type::Number`, and
`opt_lower` emits a **bare f64 op** (no tag check, no slow block, no branch) when
every operand is proven `Number`/`Int` — the shape the per-step path reaches with
known-number provenance, so the narrowing pays off in the lowering, not just in
CSE. Measured: the arithmetic `do`/`while` holds ~1.17x (no regression), and the
new `for` loop is roughly parity/noisy — the per-step path is already good on that
shape, so this slice is *reach*, and it hands those loops a real preheader block
(the fused-test block) for a future LICM. Gates: `cargo test --workspace` green
(302 jit tests, incl. a `for`-loop e2e that asserts the tier lowered it); `cargo
clippy --workspace --all-targets -- -D warnings` clean; with `SLAG_OPT=1` test262
`language` 23,726 / 0 and `built-ins` 23,820 / 0 / 1, both at baseline.

**LICM, and the branch-truthiness fix (2026-10-07).** `pass/licm.rs` finds
natural loops from back edges (with a dominance gate so a self-loop header does
not absorb its own preheader), requires a **unique** preheader, marks the
invariant-and-speculatable instructions (a narrowed pure op, or a `FrameLoad` of
a slot never stored in the loop — neither can trap, so running them once before a
zero-trip loop is unobservable), and hoists them in dependency order. Entry-header
`do`/`while` loops are skipped (no preheader) — which is exactly why LICM needed
the fused-test slice.

The bigger find was in the lowering: `Term::Branch` ran its condition through
`Helper::ToBooleanSlow` **unconditionally** — a helper call and interpreter
round-trip on *every* branch, i.e. every loop iteration. A `Bool`-typed condition
is already a canonical boolean, so a compare against `false` suffices; that one
change flipped the tier from ~1.2x slower to faster and supercharged the CSE row.
Measured (min-of-5 interleaved): arith **~1.6x**, CSE **~4.0x**, LICM ~1.12x, `for`
~1.19x. Gates: `cargo test --workspace` green (302 jit tests; 15 pass-unit tests
incl. the LICM hoist and the entry-header refusal); `cargo clippy --workspace
--all-targets -- -D warnings` clean; with `SLAG_OPT=1` test262 `language`
23,726 / 0 and `built-ins` 23,820 / 0 / 1, both at baseline.

**Queued (both need a slice, not a patch).** (a) The fused `FastLoopHead` itself
(plus `FastLoopBind`/`FastLoopStore`) is still unlifted, and the *acc-path* form
(`FastLoopHead { Counter }`) is additionally blocked by its `RunRegBody` body —
a second front end for `LeafOp`s. `FastLoopHead { Global }` is refused by the
per-step path too. The scalar replacer is deferred. (b) **Resolved as a negative
result** (below): the tier's unconditional member-read cell probe does not pay.

**The tier is on by default (2026-10-07).** `JitEngine::new()` now enables the
optimizing tier unless `SLAG_OPT=0` (a body the lift refuses keeps the per-step
path either way), so `install` and every default run go through the lift. The
per-step lowerer's unit tests — which assert helper call *structure* via test
doubles (`slow_unary_uses_the_helper` expects the `-42` double, and the IR now
correctly folds `-1` instead) — are pinned to `with_opt(false)`. Validation in
the **default** config: test262 `language` 23,726 / 0, `built-ins`
23,820 / 0 / 1, `annexB` 1,086 / 0, `staging` 77 / 0; wasm `jsapi` 1,001 tests
0 fail. Two caveats recorded honestly: (1) the corpus A/B is too noisy on this
machine to read — two *identical* `SLAG_OPT=1` runs differ by >5% on 36 of 77
rows (up to 2x) — so the enablement rests on the interleaved micro-benchmarks
(1.1–4x on lifted shapes) plus the conformance baseline, not on a corpus-wide
net win; (2) with the default flipped, the `installed_jit_*` e2e tests now
exercise the tier, so the per-step path's e2e coverage is the pinned unit tests
plus the `SLAG_OPT=0` sweeps.

I built the tier's inline member-value cell probe in `opt_lower` (the
plain-object path of the per-step `emit_member_cell_probe`) and measured it on a
read-heavy loop (`s += o.x`, 5M iters, min-of-5 interleaved, `SLAG_OPT` 0 vs 1):
**~33.15ms with no probe vs ~32.87ms with it — under 1%, i.e. noise.** The reason
is that the fallback `Helper::GetMemberName` is a C call into
`Vm::get_member_name`, which already hits the same member cell; the tier's
unconditional call is about as cheap as the inline probe, whose tag and cell
checks are not free. So the probe is **reverted**: ~100 lines of layout-coupled
duplication for no measured gain, and §6's "do not fork" applies squarely. The
read regression the plan feared is **not real** — the tier is at parity on
reads. The lever for reads is not an *unconditional* inline probe but a
*guarded, hoisted* read whose premise (the cell's generation) the feedback
record retires — the cell as a premise, not as a straight-line mirror.

**Modulo inline, and the regression it removed (2026-10-07).** The tier's `%`
now takes the same integer fast path as the per-step lowerer: `call_mod_int` (in
`opt_lower`) mirrors `emit_mod_int` — both operands Numbers, integral, inside
the i32 range, divisor nonzero -> `srem` with the dividend's sign copied on,
else `BinarySlow`; the tag check is dropped when the narrowing pass has proven
both operands `Number`/`Int`. Before, `Op::Mod` fell straight to `BinarySlow`,
so the tier *lost* on modulo-heavy code (measured on a `%` loop, 5M iters,
min-of-5 interleaved, `SLAG_OPT` 0 vs 1: the tier was **85.7ms against 78.7ms
per-step — 1.09x slower**; after, it is **75.6ms against 76.9ms — 0.98x**, a
~1.13x improvement for the tier). `Op::Pow` stays on `BinarySlow` in *both*
paths (cranelift 0.134 has no `fpow`, and `inline_binary` excludes `Exp`), so it
is parity, not a regression. Gates: `cargo test --workspace` green (303 jit
tests, incl. a modulo e2e that exercises both the in-range fast path and the
out-of-range/fractional slow path); `cargo clippy --workspace --all-targets --
-D warnings` clean; with the default (tier on) test262 `language` 23,726 / 0 and
`built-ins` 23,820 / 0 / 1, both at baseline.

**Resume fidelity + `LoadIdent` (2026-10-07).** `Op::Check` now lowers: a
speculation guard whose `args[0]` is the condition, `args[1..]` the live operand
stack, and `imm` the resume step, emitted as `brif(cond, cont, deopt)` with the
deopt block mirroring the stack into the working region (`Abi::work`, the entry's
second parameter) and returning `DISPATCH_DEOPT`. That is the per-activation exit
the guarded, hoisted read needs. It required lifting `Step::LoadIdent` (a
`BindingLoc::Env` global read) — every body the lift previously accepted was
leaf-eligible and ran through the leaf lane, which has no deopt handling, so a
non-leaf body is what runs through `run_jit_body`, where the resume works (see
`.notes/tier-resume-fidelity.md`). The slice also fixed a latent nested-deopt bug:
`run_jit_body`'s DEOPT arm shifted the operands to the stack bottom, which
destroyed a nested certified call's carved frame. Gates: `cargo test --workspace`
5683 passed / 0 failed (incl. a `load_ident` e2e and a deopt-resume e2e);
`cargo clippy --workspace --all-targets -- -D warnings` clean; test262 `language`
23,726 / 0, `built-ins` 23,820 / 0 / 1, `annexB` 1,086 / 0, all at baseline.

## 10. Open decisions

1. **Feedback store location** — per body (`CompiledBody`) or per closure
   (the function object). **Decided (I3a): per body**, matching §5's
   `(body, step)` key; the field is a lazily-allocated `RefCell<Option<Feedback>>`
   on `CompiledBody`, like `jit_info`/`jit_calls`. The record is transient.
2. **Lift output** — SSA directly via block params (recommended: the lift
   knows predecessors, so the join phis are explicit and no separate
   SSA-construction pass is needed) or a non-SSA CFG plus a Cytron pass.
3. **Tier count** — one optimizing tier to start; a Maglev-style mid-tier
   only on a measured compile-time constraint.
4. **Lowering home** — a separate `opt_lower.rs` (recommended, so the two
   lowerings share the helper ABI explicitly) or an extension of
   `compiler.rs`.
