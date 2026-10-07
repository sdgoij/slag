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
| I3 | O | feedback records + `ICState` valve + retire hook + count probe | none (enabling) | proposed |
| I4 | B | builtin intrinsic inlining (scalars first, then array/collection) | `regexp_test`, `array_slice`, `string_indexof` | partly landed, outside the IR |
| I5 | I | trial inlining (caller-specialized records) | `method_call`, `js_call`, `closure_capture`, `hof_methods`, `apply_call` | proposed |
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

## 10. Open decisions

1. **Feedback store location** — per body (`CompiledBody`) or per closure
   (the function object). Decide at I3; the record is transient either way.
2. **Lift output** — SSA directly via block params (recommended: the lift
   knows predecessors, so the join phis are explicit and no separate
   SSA-construction pass is needed) or a non-SSA CFG plus a Cytron pass.
3. **Tier count** — one optimizing tier to start; a Maglev-style mid-tier
   only on a measured compile-time constraint.
4. **Lowering home** — a separate `opt_lower.rs` (recommended, so the two
   lowerings share the helper ABI explicitly) or an extension of
   `compiler.rs`.
