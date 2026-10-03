# Implementing the optimizing tier

`.notes/optimizing-tier-plan.md` is the design and the strategy: why a
TurboFan-shaped tier, what the three engines teach, and the stage order O →
I → E → L → T. This document is the engineering plan: where the code goes,
what the interfaces are, in what order it lands, and what proves each step.
No code lands until its increment is written down here.

## 1. Layering

```
syntax / lexer / parser
        |
     runtime            interpreter, Step compiler, CompiledBody, feedback store
        |    \
        |     \  lift: Step -> SSA
        v      v
       opt              SSA CFG IR, verifier, passes        (no Cranelift)
        |
        v
       jit              lower: IR -> Cranelift, helper ABI   (owns the ISA)
```

`opt` is the front end: the IR, the lift out of the runtime's `Step` stream,
and the passes. It depends on `crux`, `syntax` and `runtime` (for `Step`,
`CompiledBody`, `ScopeInfo`) and deliberately **not** on Cranelift.

`jit` is the back end. It gains a second lowering entry (`opt_lower.rs`)
beside the current per-step emitter, both sharing `JitHelpers`,
`JitCallContext`, `JIT_SLOW_PATHS` and the `Helper` enum. Keeping the
lowering in `jit` is what stops the ABI forking: there stay exactly one
helper table, one `JitCallContext` layout, one set of dispatch sentinels.

## 2. Module tree

```
crates/opt/src/
  ir.rs         SSA CFG, type lattice, effect sets          [landed]
  builder.rs    low-level SSA builder (block-param phis)    [landed]
  verify.rs     the invariants every pass may assume        [landed]
  print.rs      textual dump                                [landed]
  lift/mod.rs   pub fn lift(&CompiledBody) -> Result<Function, Unsupported>
  lift/stack.rs operand stack -> SSA values
  lift/cfg.rs   jump targets + fall-through -> blocks/edges
  lift/frame.rs frame slots -> SSA, phi at joins
  pass/mod.rs   the pipeline: fn(&mut Function) in order
  pass/fold.rs  constant folding
  pass/dce.rs   dead-code elimination
  pass/inline.rs
  pass/escape.rs
  pass/gvn.rs
  pass/licm.rs
  pass/typer.rs
crates/jit/src/opt_lower.rs    IR -> Cranelift (reuses the helper ABI)
crates/runtime/src/feedback.rs per-site typed records
```

`crates/opt` already exists with `ir.rs`, `builder.rs`, `verify.rs`,
`print.rs` and three tests (lattice/effects, a well-formed loop verifies,
bad uses/edges rejected). That is increment I0 and it is green under
`cargo test -p opt` and `cargo clippy --workspace --all-targets -D warnings`.

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
second invalidation scheme. A **megamorphic valve** — past K distinct shapes
at a site, the site stops speculating — is what keeps a guarded fast path
from being slower than the helper it replaced.

## 6. Increments

Each increment is independently landable. `I0` is landed; the rest are
proposals.

| id | content | targets | status |
|----|---------|---------|--------|
| I0 | IR core: `ir`/`builder`/`verify`/`print` + tests | none (foundation) | landed |
| I1 | lift (straight-line subset) + identity lowering behind `SLAG_OPT=1` | none (equivalence) | proposed |
| I2 | lift control flow: branches, then simple loops with phis | none (equivalence) | proposed |
| I3 | feedback records + count probe | none (enabling) | proposed |
| I4 | inlining | `method_call`, `js_call`, `closure_capture`, `hof_methods`, `apply_call` | proposed |
| I5 | escape analysis + scalar replacement | `destructure` 214x, `construct_churn`, `spread_assign`, `object_keys`, `json_*` | proposed |
| I6 | GVN + LICM + load elimination | `obj_prop`, `element_read/write`, `array_at`, `typed_array` | proposed |
| I7 | coarse typer driving guard elision | across I4–I6 | proposed |

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
- **Deopt fidelity.** `e.stack`, `using` disposal and inlined-frame naming
  have each broken once. A deopt must reconstruct the *unoptimized* frame
  the interpreter expects, at a step boundary, with the operand stack
  exactly as the interpreter would have it.
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

- `crates/opt/src/lift/mod.rs`: `pub fn lift(body: &CompiledBody) -> Result<Function, Unsupported>`.
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
