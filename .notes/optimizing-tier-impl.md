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
| I2 | — | lift control flow: branches **landed**; the fused `for` loop blocked on opt-path parity (§6 probe) | none (equivalence) | partly landed |
| I3 | O | feedback records + `ICState` valve + retire hook + count probe | none (enabling) | partly landed (I3a–b) |
| I4 | B | builtin intrinsic inlining (scalars first, then array/collection) | `regexp_test`, `array_slice`, `string_indexof` | partly landed, outside the IR |
| I5 | I | trial inlining (caller-specialized records) | `method_call`, `js_call`, `closure_capture`, `hof_methods`, `apply_call` | I5a/I5c-0/2a/2b/2c-i/iii-a/iii-b/iv landed; gated off (`SLAG_INLINE`+`SLAG_FEEDBACK`) |
| I6 | E | escape analysis + scalar replacement | `object_keys`, `typed_array_for_each`, `array_alloc`/`object_alloc`, `destructure`, `construct_churn` | design written (§6 I6); I6-0 (allocation lifts) next |
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

### I4 — lower `CallIntrinsic` into the IR (opened 2026-10-09; probe-first)

**Why.** Stage B's per-step `CallIntrinsic` sweep already inlines `Math.*`, `charCodeAt`, array `indexOf`/`at`/`includes`/`push`, and `Map.get`/`Set.has`/`Map.set` — a `%`-identity-gated fast path with a general-call fallback. The optimizing tier has no producer for it: the lift refuses `Step::CallIntrinsic` (`Unsupported::Step`), so a body containing one never enters the tier, and if the lift were widened without an op it would lower as an opaque `Op::Call` (the general `call_slow` helper) — a large regression against the per-step fast path. This is the same parity contract the element-read ports satisfied (parity ports 1 and 2, above): **the opt path must be at least as fast as the per-step path for every shape the lift admits**, or lifting is a pessimization. `CallIntrinsic` is the last known per-step inlining the opt path lacks.

**Probe (do first; land the probe, and stop if it says no).**
1. **Name the blocker and count bodies.** The lift's `step_name` does not list `CallIntrinsic`, so a bail reads as the generic `"step"` — add `Step::CallIntrinsic { .. } => "CallIntrinsic"`, then extend `scratch/probe_steps.sh` to aggregate **every** bail reason per body, not just the first (every corpus row's loop is fused on `BuilderBind`, so `CallIntrinsic` is never a *first* blocker and today's table cannot see it). The deciding number: how many distinct bodies (over the 77 corpus rows + `--jit-bench`) have `CallIntrinsic` as their **only** non-fused-loop blocker — those are the bodies I4a unblocks — versus how many also carry other blockers, where I4a only removes a future regression.
2. **Traffic.** Run the corpus with `JIT_HELPER_STATS=1 --corpus …` and sum the intrinsic helper counts (`ArrayIndexOf`, `MapGet`, `SetHas`, `MapSet`, `ArrayAt`, `ArrayIncludes`, `ArrayPush`, `CharCodeAt`); the `Math.*` kinds are inline (no helper), so add a targeted micro. Report per-kind observations and distinct sites. This extends I5a's finding that the non-inlinable fifth of call traffic is builtin callees (`opcost` 12.9M, `builtins` 4.8M) — I4's domain.
3. **The regression size.** One binary, a `while` body with the intrinsic firing (`s += Math.abs(a[i]);`) against the same step gate-defeated by shadowing the method (`var m = Math; m.abs = function (x) { return x < 0 ? -x : x; }; s += m.abs(a[i]);`) so the *same* `CallIntrinsic` step takes its slow general call. The gap is the per-element cost the parity port removes. Repeat for a sentinel helper (`Map.get`, `charCodeAt`).
4. **Decide.** If no liftable body is gated on `CallIntrinsic` **and** the fast/slow gap is small, record the negative and stop (the typed read's outcome); the op then waits until the fused-loop lift actually carries intrinsic bodies. If the gap is large, or the fused-loop lift will admit intrinsic bodies, proceed.

**Probe result (2026-10-09): proceed — a coverage unblock *and* a real parity gap.**
- **Coverage.** The first-blocker probe cannot see `CallIntrinsic` (every row's loop is fused), so a new `JIT_DUMP_STEPS=1` diagnostic (`crates/jit/src/compiler.rs`) prints `blocking_step_names(body)` — every step family that blocks the lift, mirroring `stack_delta` — and `scratch/probe_intrinsic.sh` unions it per row. **23 of 77 corpus rows have a `CallIntrinsic`-gated body**, and **5 rows are blocked by nothing else but `{CallIntrinsic, FusedLoop}`** (`builtins/math_intrinsics`, `opcost/math_abs`, `opcost/math_call`, `strings/char_ops`, `strings/search_slice`) — those lift the moment the fused-loop lift and I4a both land. (The rest add `ArrayLiteral`/`ObjectLiteral`/`Construct`/`ArgsBase`, which I6-0 and the residual rows cover; I4a is still a prerequisite for each.) This is a *coverage* lever, unlike the element-read parity ports.
- **Parity gap.** `scratch/intrinsic_gap.js` times the same `CallIntrinsic` step firing (`Math.abs`, gate passes) vs gate-defeated (`Math.abs` shadowed, the step takes its general call) in one binary, per-step (the lift refuses the body either way): **fast ~101ms vs slow ~145ms (~1.45x, 5M × 9, min-of-3)** — so an opt-path body lowering `CallIntrinsic` as an opaque `Op::Call` would run ~1.45x slower than the per-step fast path it replaced.

So I4a is worth writing. Land the probe first (the diagnostic + script), then the op/producer/lowering, then re-open the fused-loop lift.

**I4a status (2026-10-09): landed.** `Op::Intrinsic` (`crates/jit/src/opt/ir.rs`, `Effects::call()`) carries `Imm::Int(kind)` with `args = [this, callee, a1..aN]`; the lift emits it from `Step::CallIntrinsic` (with the `stack_delta` twin), and `opt_lower::emit_intrinsic` reproduces the per-step fast path — the `ctx.intrinsic_bits[kind]` `%`-identity gate (plus `is_double(arg1)` for `Math`/`charCodeAt` and `is_string(this)` for `charCodeAt`), an inline `Math` op (`fabs`/`ceil`/`floor`/`trunc`/`sqrt` + `canon_double`), the narrow helpers (`ArrayIndexOf`/`MapGet`/`SetHas`/`MapSet`/`ArrayAt`/`ArrayIncludes`/`ArrayPush`/`CharCodeAt`) with their sentinel fall-back, and `Helper::CallSlow` as the general-call fallback. No new helper, no new signature. Three e2e tests (`installed_jit_lifted_intrinsic_{math,array_indexof,declined_receiver...}`) assert the opt body is produced (`OPT_COMPILED`) and the value matches; jit 339/0.

**Isolated measurement: a real win.** `scratch/intrinsic_bench2.js` binds `Math` to a local (so the loop's receiver read is cheap) and runs the intrinsic + dense read in a `while` body: the opt path is **~62ms vs ~98ms for the pre-I4a per-step body (~1.6x)**.

**The blocker it exposes: the opt path reads an env-global *per loop iteration*.** `scratch/intrinsic_isolate.js`'s `mathRead` — a `while` body reading `Math.PI` (no intrinsic, no `CallIntrinsic`) — is **~804ms in the opt path vs ~74ms per-step (~11x)** on BOTH the I4a and the pre-I4a binary; the `Intrinsic`'s `Effects::call()` is not the cause (the body has no intrinsic). `scratch/global_read.js` pins the shape: a loop body reading a primitive global (`P`) and one reading an object global (`O`) are **both ~1000ms opt vs ~62-78ms per-step** — so it is the `Op::IdentLoad` itself, not the intrinsic's member read and not the object-vs-primitive distinction. The lift lowers a global read to `Op::IdentLoad`; the opt LICM pass cannot hoist it (`hoistable` requires `effects.is_pure()`, and `IdentLoad` reads `Heap::Globals`), and the per-step path's dedicated global-read hoist — the `installed_jit_hoists_invariant_global_reads_without_changing_semantics` test, whose bodies are fused `for` loops that run per-step — was never ported to the opt path. This is pre-existing and out of I4a's scope, but it is a **prerequisite for widening the lift on any global-reading body** — including the very intrinsic rows I4a unblocks (`opcost/math_abs`, `math_call`, … all read `Math` per iteration), which would regress ~11-16x if lifted as-is. The next lever is therefore the opt path's global-read hoist (the per-step mechanism ported to the IR, or a cheaper cell-backed `load_ident`), NOT a wider lift.

**The op and the producer (if proceeding).**
- `Op::Intrinsic` with `Imm::Int(kind)` (the `Intrinsic` discriminant) and `args = [this, callee, a1..aN]`, mirroring `Op::Call`. `default_effects = Effects::call()` for this slice — the op carries no speculative premise yet, so no pass may hoist it (that is I4b).
- The lift: `Step::CallIntrinsic { kind, argc }` → `Op::Intrinsic`, plus the `stack_delta` twin (`-(argc + 2) + 1`). The `max_stack_usage` arithmetic already covers the step.
- The lowering (`opt_lower`), mirroring the per-step arm: load `ctx.intrinsic_bits[kind]`; gate on `non_zero & (callee == expected)`; for `StringCharCodeAt` also `is_string(this)` and `is_double(arg1)`, for `Math*` also `is_double(arg1)`, the collection/array kinds on the callee identity alone (the helper validates the receiver). `brif(gate, fast, [], slow)`; `fast` computes — `Math` inline (`fabs`/`ceil`/`floor`/`trunc`/`sqrt` + `canon_double`; `MathSqrt` is the default arm), the sentinel helpers via `abi.sig_unary` (2-value: `MapGet`, `SetHas`, `ArrayAt`, `ArrayPush`, `CharCodeAt`) or `abi.sig_binary` (3-value: `ArrayIndexOf`, `MapSet`, `ArrayIncludes`), a sentinel result routing to `slow`; `def_var` → merge. `slow` runs the general call — the same `Helper::CallSlow` `Op::Call` uses, materializing the args at `abi.work`. No new helper and no new signature.
- **The `intrinsic_bits` snapshot is already keyed on the body**: `CompiledBody::has_call_intrinsic` drives `intrinsic_bits(agent)` into the ctx, and a lifted body is compiled from the same `CompiledBody`, so the ctx carries the snapshot.

**Tests.** A `while`-body e2e per family — `Math` inline (no helper), a sentinel helper (`Map.get` / `array.indexOf` / `charCodeAt`), and the gate-defeated shape (a shadowed method) falling back to the general call and matching the interpreter. Where a helper-count wrapper can prove the inline path (as the element-read tests count `get_member_computed`), use it.

**Gate.** §6's gate. Ordering: land I4a, **then** re-open the fused-loop lift with both element-read arms and I4a in place — the lift-widening contract is then satisfied for the dense read, the typed read, and the intrinsic.

### Parity port 3 — the inline global read (`Op::IdentLoad` / `Op::GlobalLoad`) (opened 2026-10-09; probe-first)

**Why.** The per-step path inlines the direct-mapped global-value fast cell for both `LoadGlobal` (`compiler.rs::emit_global_read` → `emit_global_read_probe`) and `LoadIdent` (the same probe plus the `globals_unshadowed` gate), falling back to `get_global`/`load_ident` on a miss. The opt path has no such probe: `opt_lower` lowers `Op::GlobalLoad` through `Helper::GetGlobal` and `Op::IdentLoad` through `Helper::LoadIdent` — an unconditional C call per read (the `Op::IdentLoad` comment even says "the global-value cell fast path is a later slice"). So a lifted loop body reading a global pays a full env-chain resolve every iteration. This is the same parity contract the two element-read ports satisfied, and it is the last read-parity gap before the fused-loop lift can widen: **every one of I4a's intrinsic rows reads `Math` (a global) in its loop**, so lifting them as-is regresses them.

**Probe (measured 2026-10-09; the shape is plain).**
- `scratch/global_read.js`: a `while` body reading a primitive global (`P`) and one reading an object global (`O`) are **both ~1000ms in the opt path vs ~62–78ms per-step (~13-16x)**, on the I4a and pre-I4a binaries alike. `scratch/intrinsic_isolate.js`'s `mathRead` (`Math.PI`) is ~804ms vs ~74ms. It is the read, not the receiver: a `while` body has no compiler-emitted `HoistGlobalGuard` (that is a `for`-loop feature, `compile_hoisted_member_for`), so the per-step 62ms is the inline cell probe and the opt 1000ms is the helper call.
- The fallback really is a heavy resolve (an env/global walk), unlike the reverted member-cell probe whose fallback `get_member_name` already hit the same cell — so the inline probe should pay here.
- **Decide: proceed.** A liftable `while` body reading a top-level `var` is ~16x slower under the (default-on) opt tier — a live regression, not just a corpus one.

**The port.** Add a shared `emit_global_read(builder, helpers, abi, atom, gated, fallback)` in `opt_lower`, mirroring `compiler.rs::emit_global_read_probe`: load `ctx.global_object` and `ctx.global_value_cells`; when `gated`, also require `ctx.globals_unshadowed != 0`; require `global != 0`; probe the cell (`cell_name == atom`, `global_id == live id`, `generation == live gen`) → the cached value, else `fallback`. `Op::GlobalLoad` → `fallback = Helper::GetGlobal`, not gated; `Op::IdentLoad` → `fallback = Helper::LoadIdent`, gated. Reuses the existing helper sigs (`abi.sig_bool` = vm + name); no new helper.

**Tests.** A `while` body reading a global, asserting the value matches the interpreter and that the fallback helper runs under a threshold (a counting wrapper on `LoadIdent`/`GetGlobal`, as the element-read tests count `get_member_computed`); a shadowed global (the `globals_unshadowed` gate must miss so the shadowed value is read); and a mid-loop global mutation (the generation bump must miss to the helper).

**Gate.** §6's gate. **Ordering:** land this before re-opening the fused-loop lift; it is the last read-parity port and directly enables the intrinsic rows.

**Status (2026-10-09): landed.** `opt_lower::emit_global_read` mirrors `emit_global_read_probe` (the cell name/id/generation validation, the `global_object`/`global_value_cells` loads, the optional `globals_unshadowed` gate), and both `Op::GlobalLoad` (ungated → `GetGlobal`) and `Op::IdentLoad` (gated → `LoadIdent`) lower through it. Measured (`scratch/global_read.js`, `scratch/intrinsic_isolate.js`, `scratch/intrinsic_bench.js`): `globalPrim` **~1000ms → 35ms** (per-step 59ms), `globalObj` **~1000ms → 36ms** (per-step 76ms), `mathRead` **~804ms → 38ms**, and the global-`Math` intrinsic body `withAbs` **~855ms → 66ms** — so the opt path now beats the per-step path on global reads and the ~13-23x regression is gone. Gate green: jit 340/0 (`installed_jit_lifted_global_read_inlines` counts `load_ident` under 100); `cargo test --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; test262 `language` 23,726/0/0/0 and `built-ins` 23,820/0/1/0; `scratch/global_read_port.js` identical under `--jitless`/`--gc-stress`/`--gc-verify`/`--nursery-stress`. The fused-loop lift can now be re-opened: the read-parity set (dense element, typed element, intrinsic, global) is complete.

### The fused-loop lift — the lift-widening slice (opened 2026-10-09; probe-first)

**Why.** All 74 corpus rows' hot bodies are fused `for` loops: the lift refuses `BuilderBind`/`BuilderStore`/`FastLoopBind`/`FastLoopStore`/`FastLoopHead`/`RunRegBody`/`PushAcc`/`PopAcc`/`IncAcc`/`DecAcc`, so no corpus row enters the tier. The first attempt lifted it and was reverted because it *regressed the element-read tests* (§6, "blocked on opt-path parity") — the opt path then called the `get_member_computed` helper per element. The read-parity ports (dense element, typed element, intrinsic, global) have since landed, so the regression that reverted it is closed.

**Probe (2026-10-09): the shapes.** `scratch/probe_fused.sh` with the new `shape_census` diagnostic (`JIT_DUMP_STEPS=1`, `crates/jit/src/opt/lift/mod.rs`) — the distinct step-variant and `RunRegBody` `LeafOp` names per bailed body. 74 rows gated on `FusedLoop`. Machinery: `FastLoopHead` 74, `FastLoopBind`/`FastLoopStore`/`BuilderBind`/`BuilderStore` 71, `RunRegBody` 59, `PushAcc` 30. Leaf ops (rows using each): `LoadCounter` 43, `BinReg` 42, `StoreReg` 40, `BinStoreReg` 20, `BinImm` 18, `GetMemberNameLocal` 14, `LoadReg` 11, `BinLeftReg` 7, `BinImmLocal` 7, `LoadConst` 5, `StoreMemberName` 4, `PushAcc`/`BinAccPop` 4, `StoreMemberComputedSlot` 3, `GetMemberComputedLocal` 2, `BinStoreNum`/`BinStoreInt` 2, `StoreMemberNameLocal`/`GetMemberName`/`BuilderAppend`/`BinConst` 1. So the subset that unblocks the arithmetic rows is small; the member **stores** are the tail (and the opt path has no `Op::ElementStore`/`Op::MemberStore` lowering yet — §6, the `lower_inst` arm list).

**The reshape: the counter *is* the binding slot.** The interpreter keeps the fused counter in a dedicated `Vm` field in-loop and writes it back at `FastLoopStore`, and the compiler redirects every in-loop counter access to the Acc steps (a normal `LoadLocal` of the counter is never emitted in-loop). So the lift can model the counter as the binding's frame slot directly, and the reshape is:
- `BuilderBind { slot: None }` / `BuilderStore { slot: None }` → no-op (the `Some` string-builder variants refuse — a later slice).
- `FastLoopBind { var }` / `FastLoopStore { var }` → no-op (the binding slot already holds the counter; `FastLoopHead` keeps it current).
- `FastLoopHead { var, op, limit, inc, body_start, after }` → a terminator over the counter's binding slot: store `slot = slot ± inc`, test `slot op limit` (the fused relational test, TDZ-free frame slot), branch to `body_start`/`after`.
- `PushAcc`/`PopAcc`/`IncAcc`/`DecAcc` → frame-slot accesses on the counter's slot (push the slot, store the popped value, `slot ± 1`).
- `RunRegBody { ops }` → lower each `LeafOp` to IR ops, with `Vm.acc` as an SSA value local to the run (the interpreter treats `acc` as scratch and truncates the stack after).

**Slices.**
- **S1 — the scaffold** (`BuilderBind`/`BuilderStore`/`FastLoopBind`/`FastLoopStore` no-ops, the `FastLoopHead` terminator, `PushAcc`/`PopAcc`/`IncAcc`/`DecAcc`): lifts `for (var i = 0; i < n; i++) { s = s + i; }` — the canonical body the reverted attempt already lifted.
- **S2 — `RunRegBody`, the arithmetic subset**: `LoadCounter`, `LoadReg`, `BinReg`, `BinLeftReg`, `BinImm`, `BinImmLocal`, `StoreReg`, `BinStoreReg`, `LoadConst`. The largest unblock.
- **S3 — the member leaf ops + `PushAcc`/`BinAccPop`**: `GetMemberNameLocal`/`GetMemberComputedLocal` (→ `Op::MemberLoad`/`Op::MemberCellLoad`+`MemberGuard`, `Op::ElementLoad`), `PushAcc` (leaf) / `BinAccPop`. Read-only: these lift the read rows.
- **S4 — the member stores** (`StoreMemberName`/`StoreMemberNameLocal`/`StoreMemberComputedSlot`): needs a new opt-path store lowering (the `Op::MemberStore`/`Op::ElementStore` port) — its own slice; the ~8 store rows stay bailed until then.

**The gate is the read-parity contract, and the tests encode it.** `installed_jit_dense_array_element_read_inlines` and `installed_jit_typed_array_element_read_inlines` are read-only `for` bodies — with the fused loop lifted they must still take the inline reads (that is exactly what reverted the first attempt), so they are the regression net. Add e2e tests per slice (a canonical numeric `for`, an array-element `for`, an intrinsic `for`) asserting the value matches and the opt body is produced (`OPT_COMPILED`).

**Ordering.** S1+S2 first (the scaffold + the arithmetic body — unblocks the arithmetic rows and is the reverted attempt's shape, now safe). S3 next. S4 is a separate store-port slice.

**S1+S2 status (2026-10-09): landed.** The lift accepts the fused canonical loop: `BuilderBind{None}`/`BuilderStore{None}`/`FastLoopBind{num:None}`/`FastLoopStore{num:None}` are no-ops, `FastLoopHead` is a terminator over the counter's frame slot (`emit_fused_test`), the Acc steps (`PushAcc`/`PopAcc`/`IncAcc`/`DecAcc`) read/write that slot, and `RunRegBody` lowers its `LeafOp`s to generic IR ops (mirroring `seed_acc_undefined` + `emit_leaf_op`, the register path's numeric provenance being a pure optimization). Only the acc-path (`FastLoopVar::Counter`) head is accepted — its compile gate proves the init a Number, so `slot ± 1`/`slot op limit` are exact; the general `Slot`/`Global` head, the `Some` builder/`num` variants, and the remaining leaf ops (member stores, `BinAccPop`, `UpdateAcc`, the context reads) refuse, so those bodies keep the per-step path.

**Coverage: the FusedLoop blocker is gone.** `scratch/probe_intrinsic.sh` (with the `shape_census`/`blocking_step_names` diagnostics) now reports **0 rows gated on `FusedLoop`** (was 74) and 0 on `CallIntrinsic`; the remaining blockers are the allocation literals (`ArrayLiteral` 26, `ObjectLiteral` 18 — I6-0) and other unnamed steps. Six `opcost`/`arrays` rows fully lift (`array_alloc`, `baseline`, `math_abs`, `math_call`, `num_valueof`, `object_alloc`); the arithmetic rows still blocked elsewhere lose the fused-loop gate too. Parity holds: `scratch/fused_math_bench.js` (the `math_abs` shape × 50) is opt 56-57ms vs per-step 57ms; the two element-read `for` tests are the regression net (they pass — the opt path inlines). Gate green: jit 342/0; `cargo test --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; test262 `language` 23,726/0/0/0 and `built-ins` 23,820/0/1/0; `scratch/fused_port.js` identical under `--jitless`/`--gc-stress`/`--gc-verify`/`--nursery-stress`.

**Next: I6-0 (allocation lifts) is the actual corpus unblock now** — the fused loop was the *first* blocker; the literal families are the next. S3 (member reads) and S4 (stores) widen the register-body subset.

### I6-0 — lift the literal creates (opened 2026-10-09; probe-first)

**Why.** With the fused loop landed, the literal families are the dominant blocker: 41 corpus rows are gated on `ArrayLiteral`/`ObjectLiteral`, and ~20 of them on a literal family *alone*. The lift refuses every literal step, so any body that creates an object or array never enters the tier. `Op::NewObject`/`Op::NewArray` (and `ElementStore`/`MemberStore`) are declared in `ir.rs` and never produced or lowered — I6-0 is the ground E (escape analysis) stands on, not an optimization.

**Probe (2026-10-09): the fused forms dominate.** `scratch/probe_literals.sh` (the `shape_census` diagnostic, filtered to the literal-gated rows): of 41 rows, `ArrayFast` 20, `ObjectFast` 15, `ArrayBegin` 6, `ArrayEnd` 6, `ObjectBegin` 4, `ObjectInitName` 2. So **I6-0a (the two whole-literal fused creates) covers 35 of 41**; the non-fused incremental forms (`ArrayBegin`/`Element`/`End`, `ObjectBegin`/`InitName`) are the tail and need `Op::ElementStore`/`Op::MemberStore` (I6-0b).

**I6-0a — the fused creates (behavior-neutral).** Lift `Step::ArrayFast { count }` → `Op::NewArray` (args = the `count` element values, `Imm::Int(step)`) and `Step::ObjectFast { names }` → `Op::NewObject` (args = the `names.len()` values, `Imm::Int(step)`). Lower each by materializing the values at `abi.work[0..n)` and calling the **same** fused helper the per-step path calls: `Helper::ArrayFast(count, work + 8n)` / `Helper::ObjectFast(step, work + 8n)`, which read the n consumed values *below* the `sp` (`sp - 8n .. sp`). `ObjectFast`'s `names` payload is read back from the running body via the step index (`step_at(ctx, step)`), exactly as the per-step path does — the opt body is compiled from the same `CompiledBody`. `abi.sig_unary` (vm + 2); no new helper, no new semantics. Stack effect `-(n) + 1`.

**I6-0b — the non-fused forms (the tail).** `ArrayBegin`/`ArrayElement`/`ArrayHole`/`ArraySpread`/`ArrayEnd` and `ObjectBegin`/`ObjectInitName`/`ObjectInitComputed`/`ObjectKeyToPropertyKey`/`ObjectSpread` → the same `Op::NewArray`/`NewObject` opens plus `Op::ElementStore`/`Op::MemberStore` for the inits (whose opt lowering is a separate port; the per-step path uses `array_element`/`object_init_name` helpers with the object riding the work stack). Later slice.

**Tests.** An e2e `for` body creating an array literal and an object literal per iteration (a lifting body), asserting the opt body is produced (`OPT_COMPILED`) and the aggregate value matches the interpreter; the value parity over many iterations pins the fused-create correctness.

**Gate.** §6's gate. Land I6-0a, re-probe, then I6-0b as needed.

**I6-0a status (2026-10-09): landed.** The lift produces `Op::NewArray` from `Step::ArrayFast { count }` (args = the count values, `Imm::Int(step)`) and `Op::NewObject` from `Step::ObjectFast { names }` (args = the names.len() values, `Imm::Int(step)`); `opt_lower::emit_new_array`/`emit_new_object` materialize the values at `abi.work[0..n)` and call the same fused helpers the per-step path uses (`array_fast(count, work + 8n)` / `object_fast(step, work + 8n)`, which read the n values below the `sp`; `ObjectFast`'s names are read back from the running body via `step_at`). No new helper, behavior-neutral. `installed_jit_lifted_literal_creates_match_the_interpreter` asserts a `for` body creating an array + an object literal per iteration lowers through the opt path and matches.

**Coverage: 34 of 77 rows fully lift** (up from 14 — I6-0a unblocked 20); `ArrayLiteral` dropped from 26 rows to 6, `ObjectLiteral` from 18 to 4 (the non-fused forms, I6-0b). Perf: the `arrays` family is parity (min-of-3: `for_of_dense` 1.00, `push_pop` 1.01, `slice_concat` 1.00, `typed_array` 1.01, `index_loop` **1.10** faster, `hof_methods` 0.95), with large `opcost` wins across the newly-lifting rows (`math_abs` **2.43x**, `map_for_each` 1.71x, `map_get` 1.62x, `math_call` 1.36x) — replacing a per-iteration slow path now that the bodies are compiled. Gate green: jit 343/0; `cargo test --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; test262 `language` 23,726/0/0/0 and `built-ins` 23,820/0/1/0; `scratch/literal_port.js` identical under `--jitless`/`--gc-stress`/`--gc-verify`/`--nursery-stress`.

**I6-0b status (2026-10-09): landed.** The non-fused literal steps lift through the *same* helpers the per-step path calls, with the container threaded as an SSA value (the VM's `array_index_stack` tracks the element index between steps, exactly as the per-step path): `ArrayBegin`/`ArrayElement`/`ArrayEnd` → `Op::ArrayBegin`/`ArrayElement`/`ArrayEnd`, `ObjectBegin`/`ObjectInitName` → `Op::ObjectBegin`/`ObjectInitName` (the latter's `name`/`set_name`/`shorthand` payload rides as three raw-bit `Const` args). The remaining non-fused steps (`ArrayHole`/`ArraySpread`, `ObjectInitComputed`/`ObjectKeyToPropertyKey`/`ObjectSpread`) are not in the blocking census and stay refused. **No literal blocker remains** — the families are fully cleared — and rows fully lifting went 38 → **42**.

### The blocker census (2026-10-09) — the remaining coverage levers

With `blocking_step_names` reporting *variant* names (not `step_name`'s coarse families), the census is decisive — **as a stack-delta count, an upper bound, not the real lift rate (see the correction below)**. 42 of 77 rows fully lift. The rest are gated on (rows using each): `ArgsBase` 15, `Construct` 14, `CreateFunction` 13, `ArgsPush` 8, `LoadContextSlot` 7, `FunctionDeclInit` 7, `AssignMemberName` 6, `ArrayEnd`/`ArrayBegin` 6, `CallFastSlot` 5, `ObjectBegin` 4, `AssignMemberComputed` 4, then a tail. **Only 5 rows have a single blocker** and the rest are 2-4-deep (blocker-set sizes: 16 rows size 2, 8 size 3, 7 size 4), so no single small slice moves many rows — each further row needs 2-3 families cleared. The best 2-slice pairs: `{ArgsBase, Construct}` 5 rows (both heavy — the argument-vector call + construct), `{ArrayBegin, ArrayEnd}` 3 rows (I6-0b, the non-fused array literal), `{HoistMemberGuard, HoistGlobalGuard}` 2 rows (cheap).

**Hoist-guard no-op (2026-10-09): landed** — the cheapest slice in that list. `HoistMemberGuard`/`HoistGlobalGuard` are pure compiler perf-guards: on a hit the guarded copy runs with a hoisted value, on a miss the general copy, and both compute the same result. The lift models the guard as **always-miss** (`emit_fused_test` returns a constant-false hit condition, so the branch takes the miss target and the guarded copy is lifted but never taken). One look-alike must NOT be treated this way: `TypedArrayLengthHoist` pops the receiver on a hit, so an always-miss model would leave it on the stack — it stays refused. Rows fully lifting: 34 → 38. `installed_jit_lifted_hoist_guard_matches_the_interpreter` (an `o.x` loop, whose shape carries a `HoistMemberGuard`) asserts the opt body is produced and matches; gate green (jit 344/0, workspace, clippy, test262 `language` 23,726/0/0/0 + `built-ins` 23,820/0/1/0, `scratch/hoist_port.js` identical under the gc differentials).

### The vector construct — `ArgsBase`/`ArgsPush`/`Construct` (opened 2026-10-09; probe-first)

**Why.** The argument-vector family is the largest remaining blocker. Every `ArgsBase` row is a `Construct` row (the corpus uses the vector form for `new X(...)`, not for calls — a zero-arg `new Map()` is `ArgsBase; Construct`), and `{ArgsBase, Construct}` is the unique biggest 2-blocker pair (5 rows), plus `ArgsPush`/`ArgsSpread` for multi-arg/spread constructions.

**Probe (2026-10-09).** 14 rows carry `ArgsBase`+`Construct`; the sets are `{ArgsBase, Construct}` (5 rows: `map_get`/`map_set`/`set_has`/`map_churn`/`set_churn`), and larger sets add `ArgsPush`/`AssignMemberName`/`CreateFunction`. No corpus row uses the vector *call* form.

**The port (behavior-neutral).** The vector form keeps its argument vector in the VM (`Vm::args` + `args_base_stack`), built and consumed by helpers, so the lift threads the *container of the vector* through the VM, not the IR: `Step::ArgsBase` → `Op::ArgsBase` (void), `Step::ArgsPush` → `Op::ArgsPush` (args `[value]`, void), `Step::ArgsSpread` → `Op::ArgsSpread` (args `[iterable]`, void), each calling the same helper the per-step path calls (`args_base`/`args_push`/`args_spread`). `Step::Construct { span }` → the existing `Op::Construct` (args `[callee]`), lowered through `Helper::Construct(callee, sp)` — the `sp` is the current working pointer (`abi.work`), the same carve base the per-step passes for the shared-ctx leaf-construct lane.

**The `sp` is soft.** `try_shared_construct_leaf` only uses `sp` as the carve base and returns `Ok(None)` (falling back to `run_jit_leaf`) when the frame + stack do not fit above it — so any valid `sp` is safe; `abi.work` (the base of the caller's working region, which the opt path only uses transiently per op) gives the most room and cannot corrupt the caller's registers/frame. Stack effects (from `max_stack_usage`): `ArgsBase` 0, `ArgsPush`/`ArgsSpread` -1, `Construct` 0.

**Tests.** An e2e `for` body constructing `new Map()`/`new Set()` (the `{ArgsBase, Construct}` shape) plus one using `push`/`get`, asserting the opt body is produced and the value matches; a multi-arg construction exercises `ArgsPush`.

**Gate.** §6's gate.

**I-vector status (2026-10-09): landed — but as a prerequisite, not a coverage win.** The lift produces `Op::ArgsBase`/`Op::ArgsPush`/`Op::ArgsSpread` (void; the argument vector lives in the VM, not the IR) and `Op::Construct` (callee → value); the lowering calls the same `args_base`/`args_push`/`args_spread` helpers the per-step path calls and `Helper::Construct(callee, sp = abi.work)` (a soft carve base; an out-of-room `sp` falls back to `run_jit_leaf`). The ops do lower correctly: a straight-line body constructing `new Map()`/`new Set([1,2,3])`, called past the compile threshold, produces `ArgsBase`/`ArgsPush`/`Construct` in the IR (`installed_jit_lifted_vector_construct_matches_the_interpreter` — a straight-line shape, because a `for`-loop body is refused *before* the vector steps, see below).

**The outcome is a negative on coverage.** `0` corpus rows newly lift. The five `{ArgsBase, Construct}` rows (`map_get`/`map_set`/`set_has`/`map_churn`/`set_churn`) are refused earlier, at the **fused-loop `Some`-slot gate** — their `FastLoopHead` counter is not the accumulator (`Counter`) form, and their `FastLoopBind`/`FastLoopStore`/`BuilderBind`/`BuilderStore` use the `Some` (Route-B `num` / string-builder) slots the lift refuses — so they never reach the vector ops. The slice is the necessary groundwork for those rows once that gate widens, but by itself it moves no corpus row.

**Census correction.** The census above counts `blocking_step_names`, which is `stack_delta`-level: it sees a step only when the lift's *stack-delta* model refuses it, not when `emit_step` refuses a *variant* of an otherwise-modelled step. The fused-loop gate (`FusedLoop` = the `Some`-slot variants and the non-`Counter` `FastLoopHead`) is exactly such an emit-time refusal, so it is **invisible** to the census and the census over-reports coverage. The honest metric is the IR actually produced (`JIT_DUMP_IR=1`, counted by `scratch/probe_liftrate.sh`): **11 of 77 rows produce any lifted IR**, and the leading real blocker is the fused-loop `Some`-slot gate, not the argument vector. Use the IR/CLIF dump, not `blocking_step_names`, when the question is "did this row actually lift".

### The opt-tier test vacuity — fixed (2026-10-09)

The four `installed_jit_lifted_*` tests for the fused-for (S1+S2), the hoist guard, and the I6-0 literal creates were **vacuous**: each asserted a *process-global* `OPT_COMPILED` delta that a parallel test's cache satisfied, so they passed while their own body never lifted (all four failed under `--test-threads=1`). Root causes and the fix:

- **`JitEngine::compile_counted` + `JitCache::opt_compiled_count`.** The compile path now reports whether the optimizing tier produced the code, and the cache counts it per cache. A per-cache count is immune to the global counter's cross-test pollution, so an assertion on it fails when the body under test does not lift; `with_opt_jit_agent_counts` returns it.
- **The literal tests (I6-0a/b) now lift for real.** Both used a `for`-loop body, refused at the fused-loop gate, so they never reached the literal ops. They now use a straight-line body called past `JIT_COMPILE_THRESHOLD` (which the lift accepts), whose IR carries `NewArray`/`NewObject` and `ArrayBegin`/`ArrayEnd`/`ObjectBegin` respectively.
- **Both fallback tests flipped back.** The fused-for test (`installed_jit_lifted_for_loop_matches_the_interpreter`) flipped at F0/F1/F2, and the hoist-guard test (`installed_jit_lifted_hoist_guard_matches_the_interpreter`) at F3a — each failing its `opt == 0` assertion exactly when its shape started lifting, which is the forcing function working. No `..._falls_back_...` test remains.

### F — the fused-loop head widening (opened 2026-10-09; probe-first)

**Why.** This is the real coverage wall. The first-`opt bail` histogram over the 77 rows is `step` 26, `FastLoopHead` 24, `Push` 11, `RunRegBody` 10 — and *no* corpus row's lifted IR contains a single fused-loop op. The ordinary `for (var i = 0; i < n; i++)` — the dominant corpus shape — is refused, so the I6-0, I4, and I-vector work sits behind it.

**The probe (2026-10-09).** Dumping a loop's steps (`f1` = `for (var i = 0; i < n; i++) { s = s + 1; }` — acc-path, `None` slots, a fully-handled register body) shows the cause is not the head gate at all. The counter is a **first-pass linear variable** (`counter` in `lift_impl`), but the `FastLoopHead` is a *terminator*, emitted in the **second pass** (`TermRec::FusedTest`) — after the first pass has already walked the loop-exit block and cleared `counter` with `FastLoopStore`. So every fused loop's head reads `counter == None` and bails at `counter.ok_or(Unsupported::Step("FastLoopHead"))`. The 24 rows that name `FastLoopHead` are this bug, not the head's `var`; rows that bail on the `Some` slots or a leaf op bail in pass 1 and never reach the head (which is why the ones reported as "reached `RunRegBody`" are exactly the ones whose `Some`/leaf gate fired first).

**F0 — capture the counter per block.** Record the in-scope `counter` on `TermRec::FusedTest` at terminator time (pass 1) and use that captured value in pass 2, not the end-of-pass value. Behaviour-neutral where the counter is already right (an acc-path loop whose bind precedes its own head), and it unblocks the acc-path loops with `None` slots and a fully-handled register body.

**F0 status (2026-10-09): landed.** Lifted corpus bodies **14 → 37**; rows producing any IR **11 → 36** (23 now lift every body); the `FastLoopHead` first-`opt bail` count **24 → 1**. Gate green: jit 346/0 (parallel and `--test-threads=1`), `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, test262 `language` 23,726/0/0/0 and `built-ins` 23,820/0/1/0, and `scratch/loop_port.js` identical (5000100046) under `--jitless`/`--gc-stress`/`--gc-verify`/`--nursery-stress`. The remaining first-bails are `step` 26, `Push` 11, `RunRegBody` 10 (the register-body leaf set), `FusedLoop` 3 (the `Some` slots), `FastLoopHead` 1 — i.e. F1-F3 next.

**F1/F2 status (2026-10-09): landed.** `FastLoopBind/Store { num: Some(_) }` (the Route-B mirror) are now no-ops, and `LeafOp::BinStoreNum` lowers to the frame-slot RMW it mirrors. This clears the `FusedLoop` slot gate (**3 → 0**) and lifts the canonical numeric fused `for` (`s = s + i`): `installed_jit_lifted_for_loop_matches_the_interpreter` flipped back to an opt-path assertion (the F0/test-vacuity forcing function fired as designed). The corpus total is unchanged (**37** bodies) because the three rows that moved off `FusedLoop` now reach the register body and bail on its **leaf set** — the first-bail histogram is now `step` 26, `RunRegBody` 13, `Push` 11. The leaf inventory blocking the 13 `RunRegBody` rows: `GetMemberNameLocal` (10 rows), the member stores (`StoreMemberName`/`StoreMemberNameLocal`/`StoreMemberComputedSlot`), `PushAcc`/`BinAccPop` (4), and `BinStoreInt` (2). `installed_jit_hoist_guard_...` stays a fallback test (its `GetMemberNameLocal` leaf is still unhandled). F3 is the member leaves.

**F3a status (2026-10-09): landed.** Handle the register-body member read `LeafOp::GetMemberNameLocal` (the same inline cell probe + guard as `Step::GetMemberName`) and the operand-stack spill `PushAcc`/`BinAccPop`. Lifted corpus bodies **37 → 42**; rows producing IR **36 → 41** (28 now lift every body); `RunRegBody` first-bails **13 → 8**. `installed_jit_hoist_guard_matches_the_interpreter` flipped back to an opt-path assertion (its leaf set is now handled) — both fallback tests are now gone. What remains: the member **stores** (`AssignMemberName`/`AssignMemberComputed` are unhandled at the `Step` level — part of the coarse `step` 26 — and `StoreMemberName`/`StoreMemberNameLocal`/`StoreMemberComputed*` at the leaf level need `Op::MemberStore`/`Op::ElementStore`, which `opt_lower` does not lower yet), the calls/closures tail (`CallFast`/`CallFastSlot`/`CallApply`/`CallIntrinsic`, `CreateFunction`/`FunctionDeclInit`/context slots), the certified for-of/for-in steps, `Push` 11, and `BinStoreInt`/`StoreMemberComputedSlot`.

**F1 — the `Some` slots.** `FastLoopBind/Store { num: Some(_) }` is the Route-B mirror (`Vm::loop_num` holds the accumulator frame slot `s`); model it as a no-op, exactly as the `None` form. `BuilderBind/Store { slot: Some(_) }` is the string builder — refuse until its own slice.

**F2 — the Route-B leaves.** `LeafOp::BinStoreNum`/`BinStoreInt` (`loop_num = loop_num op rhs`) model as the frame-slot RMW they mirror (`BinStoreReg` semantics), sound because `plan_loop_num`/`plan_loop_int` prove the slot has no other reference while the loop runs.

**F3 — the member leaves.** `GetMemberNameLocal`/`StoreMemberComputedSlot` via the same member helpers the per-step path uses.

**F4 — the general fused head.** `FastLoopHead { var: Slot | Global }` (the path `f2`/`g1` take): the counter is a frame slot, so model the increment as a `FrameStore` and the test with the same `slot_load`/`emit_fused_test` machinery the acc path already uses.

**Measurement.** `scratch/probe_liftrate.sh` (rows producing IR) plus the first-`opt bail` histogram; F0 should clear the `FastLoopHead` rows. Gate: §6, plus the two `..._falls_back_and_matches_the_interpreter` tests flipping back to opt-path assertions when their shape (the fused `for`, the hoist guard) lands.

**F3b — the member-store port (opened 2026-10-09; probe-first).**

The eight `RunRegBody` first-bails, and a slice of the coarse `step` 26, are member **stores**: `AssignMemberName`/`AssignMemberComputed` are unhandled at the `Step` level entirely, and the register-body store leaves (`StoreMemberName`/`StoreMemberNameLocal`/`StoreMemberComputedSlot`/`StoreMemberComputed`/`StoreMemberComputedLocal`) need `Op::MemberStore`/`Op::ElementStore`, which the opt IR defines but `opt_lower` never lowers.

**The parity trap.** A lowering that calls the authoritative `SetMemberName`/`SetMemberComputed` helpers is *correct* but **violates the read-parity contract**: the per-step register path lowers the same leaf through `emit_validated_member_store` (the member-value cell probe → narrow `set_member_slot`) and `emit_dense_array_append_inline` (the dense append), so a helper-only opt lowering is *slower* than the per-step path for exactly the hot store shapes the lift would admit. The port must reproduce the inline machinery instead: the member-cell/shape-gate probe → `set_member_slot`, the deferred inline field store, the dense-array append, and the typed-array element store — the `Lowerer` methods `emit_validated_member_store`/`emit_dense_array_append_inline`/`emit_element_store` lifted into (or shared with) `opt_lower`.

**Design.** (1) `Op::MemberStore { object, value }` with `Imm::Atom(name)` → the validated name store (`StoreMemberName`/`StoreMemberNameLocal`). (2) `Op::ElementStore { object, key, value }` → the dense-append/typed/element store (`StoreMemberComputed*`). (3) The lift arms for the store leaves plus the `Step::AssignMember*` forms (the latter also thread the completion register — `SetCompletion`). (4) `RegOperand` resolution in a leaf arm: `Reg`/`Const` resolve, `Ctx`/`PerIter` refuse (their own slice).

**Acceptance.** The two element-read regression tests (`installed_jit_lifted_dense_array_element_read_inlines`, `installed_jit_lifted_for_element_read_matches_the_interpreter`) plus the corpus store rows (`element_write`, `dyn_key_read`, `index_loop`, `baseline`) at parity (min-of-3), the §6 gate, and the eight `RunRegBody` rows converting.

**F3b status (2026-10-09): landed (the name stores).** `Op::MemberStore` now lowers to the validated store — the member-value cell probe → the narrow `set_member_slot`, falling back to the full `set_member_name` helper (`emit_member_store`, mirroring the per-step `emit_validated_member_store`'s core) — and the lift handles `LeafOp::StoreMemberName`/`StoreMemberNameLocal` (with an `emit_reg_operand` resolver for `Reg`/`Const`/`Counter`; the store is effect-only and leaves the accumulator unchanged, matching `IntProvenance`). Lifted corpus bodies **42 → 47**; rows producing IR **41 → 46** (33 now lift every body); `RunRegBody` first-bails **8 → 3**. Parity (min-of-3, `scratch/store_bench.js`, a frame-slot store loop): opt **84ms** vs per-step (`SLAG_OPT=0`) **94ms**; `scratch/store_port.js` identical (1955057703) under the four GC modes; sweeps at baseline. **Not yet ported, so this is not full parity:** the shape-gate side path (an object whose store working set exceeds the value cells) and the fully-inline vector-free field store — an object with a very large store working set is served by the full helper in the opt path where the per-step uses the shape gate, a follow-up. The computed store (`StoreMemberComputed*` → `Op::ElementStore`) and `BinStoreInt` remain (the last 3 `RunRegBody` rows: `index_loop`, `baseline`, `object_alloc`).

**F3c status (2026-10-09): landed.** `LeafOp::BinStoreInt` (Route B int32: `loop_num = (i32(loop_num) op rhs) & mask`) models the frame RMW in int32 — `ToInt32` as `| 0`, the wrapping op as the f64 op then `| 0`, and `& mask` as the trailing truncation; the unproven `CounterChecked` rhs keeps the exact f64 op then `ToInt32` (the counter may be outside int32), the others a proven i32. Lifted corpus bodies **47 → 49**; rows producing IR **46 → 48** (`baseline`, `object_alloc`). Only `index_loop`'s computed store (`StoreMemberComputed*` → `Op::ElementStore`) remains in the register-body leaf set.

**F3d status (2026-10-09): landed (the computed store).** `Op::ElementStore` lowers via the authoritative `set_member_computed` helper, and the lift handles `LeafOp::StoreMemberComputedSlot` (object = frame slot, key/value `RegOperand`s). Lifted corpus bodies **49 → 50**; rows producing IR **48 → 49** (`index_loop`). Parity (min-of-3, `scratch/index_bench.js`): opt **60ms** vs per-step **64ms**. The dense-append / typed-array / dense-element-store gates are NOT ported (each is ~250 lines); the helper is at parity for the plain dense-element-store shape, but an append or typed-array store shape would be slower in the opt path — a follow-up. The register-body leaf set is now complete.

**Negatives (2026-10-09).** Two cheap widenings measured and reverted: (1) accepting `undefined`/`null` in `constant()` — 0 new corpus bodies and 6 test breaks (the `Push(non-numeric-constant)` rows bail on other gaps); (2) the plain `Step::AssignMemberName` → `Op::MemberStore` arm — 0 new bodies and 2 rows newly `Malformed` (admitting the step lets a fused-loop body reach the `FastLoopHead after is not the fall-through` check). The coarse `step` tail is calls/closures/context, not member stores.

**F5 status (2026-10-09): landed (the context/environment lift).** Add `Op::ContextLoad`/`ContextStore`/`ContextInit` (with an `Imm::Context { depth, index }`) and the lift arms for `LoadContextSlot`/`StoreContextSlot`/`InitContextSlot`, lowering through the same `load_context`/`store_context`/`init_context` helpers the per-step path's slow path calls (so the env walk stays in the helper — a lifted body pays nothing for the captured read beyond the shared call). Lifted corpus bodies **50 → 52**; rows producing IR **49 → 51**; `step` first-bails **26 → 25**. A captured-variable loop (a closure reading and writing its captured binding) now lifts (`scratch/closure_port.js`, identical 705032704 under the four GC modes). Not handled: `UpdateContextSlot` (a four-immediate helper) and `LoadPerIteration`/`StorePerIteration`. New lead: admitting `LoadContextSlot` sends `recursive_fib`'s second body to a `Malformed("unreachable block")` refusal — a safe fallback (the lift returns `Err`), but a lift invariant worth chasing.

**F6 status (2026-10-09): landed (closures).** Add `Op::NewClosure`/`Op::FunctionDecl` lowering through the same `create_function`/`create_function_decl` helpers the per-step path calls (each reads the step payload by index), and the lift arms for `Step::CreateFunction`/`FunctionDeclInit`. Lifted corpus bodies **52 → 58**; rows producing IR **51 → 52**; rows fully lifting **37 → 41**; `step` first-bails **25 → 18** (the `Push` count rose 11 → 13 — rows that used to fail at `CreateFunction` now reach the `Push(non-constant)` gap, one of the recorded negatives). A loop creating a closure per iteration and a hoisted declaration in a loop both lift; `scratch/closure2.js` identical (1105062704) under the four GC modes. Not handled: `Step::CreateArrow`.

**F7 status (2026-10-09): landed (slot/global calls).** Handle `Step::CallFastSlot`/`CallFastGlobal` (the callee from a frame slot / the global cell, `this` `undefined`) as the same opaque `Op::Call` the `CallFast` arm emits. Lifted corpus bodies **58 → 62**; rows fully lifting **41 → 44**; `step` first-bails **18 → 14** (rows were already counted, so the row total is unchanged — the newly-lifting bodies are second bodies). The call-parity caveat is measured away: the `Op::Call` (`call_slow`) path is at parity with the per-step leaf-probe call (min-of-3, `scratch/call_bench.js`: opt **84ms** vs per-step **85ms**), so widening calls does not regress.

**F8 status (2026-10-09): landed (the name store at the step level).** Handle `Step::AssignMemberName` (plain `=`) → `Op::MemberStore` (the validated store F3b ported). The stack effect is `[object, value]` → the stored value pushed back (net **-1**; the earlier `-2` attempt produced `Malformed: stack underflow`, since the per-step arm pushes the assignment result). Lifted corpus bodies **62 → 66**; rows producing IR **52 → 54**. Parity (min-of-3, `scratch/nw_bench.js`, a store-dominated loop): opt **82ms** vs per-step **94ms**.

**F8 negative.** `Step::AssignMemberComputed` → `Op::ElementStore` (`set_member_computed`) was tried and **reverted**: it regressed `element_write` (min-of-3, `scratch/ew_bench.js`: opt **3124ms** vs per-step **2494ms**, -25%), because the per-step lowerer inlines the dense element store where the helper round-trips through `set_member_computed`. The `Op::ElementStore` gap F3d noted is real and store-dominated loops expose it; admitting the computed Step form needs the inline element-store port first. The register-leaf `StoreMemberComputedSlot` (F3d) admits only `index_loop`, whose read-heavy loop measured at parity.

**F9 status (2026-10-09): landed (the inline element store).** Port the per-step `emit_dense_element_store_into` (and its `box_ptr_is_young`) into `opt_lower` as `emit_dense_element_store`/`emit_element_store`, and rewire `Op::ElementStore` through it (falling back to `set_member_computed`). Re-enable `Step::AssignMemberComputed` (plain `=`) → `Op::ElementStore`. Lifted corpus bodies **66 → 68**; rows producing IR **54 → 56** (`element_write`, `element_read`). Parity flips from the F8 regression to a win: min-of-3, `scratch/ew_bench.js`, opt **1738ms** vs per-step **2481ms** (+30%). Still not ported: the dense-append gate (`emit_dense_array_append_inline`) and the typed-array gate (`emit_typed_array_store_inline`) — an append or typed-array store shape falls to the full helper where the per-step inlines, a remaining gap.

**F10 negative (2026-10-09).** The `Push(non-constant)` gap (13 rows) is string literals (11 rows) and `undefined` (2: `closure_capture`, `recursive_fib`, whose other gaps F5/F6/F7 cleared). Accepting `undefined`/`null`/string in `constant()` lifted **+10 rows** (56 → 66) but **regressed the call-heavy ones**: five leaf-call tests fail with exactly **200000** `call_slow` calls (2 per iteration × 100000), because the opt tier's `Op::Call` lowers through `call_slow` where the per-step path uses the leaf-call probe — so admitting a call-heavy body loses the inline. This is the **I5c-2 dependency** made concrete: the opt call path must splice/inline a leaf callee before `Push(string)` bodies can be admitted at parity. Reverted (the earlier `undefined`/`null`-only attempt was also a negative: 0 bodies then, because the same rows still had the F5/F6/F7 gaps).

### G — opt-tier call parity (opened 2026-10-09; probe-first)

**Why.** F10 found that admitting a call-heavy body to the opt tier *regresses* it: the opt `Op::Call` lowers through `call_slow`, while the per-step `emit_call` first runs a runtime **leaf-call probe** that inlines any certified leaf callee in machine code. So a lifted body loses the inline — five leaf-call tests fail with exactly 200000 `call_slow` calls (2 per iteration). That blocks the 13 `Push(non-constant)` rows (11 string-heavy, 2 `undefined`) and is the real per-call cost of every already-lifted call body (F6/F7).

**What exists.** The compile-time **trial-inline splice** (I5c-2) is fully landed — the resolver, `GuardCallee`, the site map, mem2reg, recursion, the guard-free gate — but **gated off** (`SLAG_INLINE` + `SLAG_FEEDBACK`) and narrow (a call that is its block's last instruction, a monomorphic box-stable callee). It does not cover the per-step leaf probe's shapes (any certified leaf, any position), so enabling it will not close the F10 gap.

**The per-step lane, read from the code (the behaviour to reproduce).** `emit_call` (`compiler.rs` 4691):
1. **the self-call gate (G21)** — when `scope.is_some()` and `callee == ctx.current_function` (non-zero), skip the probe and take `call_slow`'s compiled-self-call path;
2. **the record slot** — `emit_leaf_record_slot(callee)` = `(callee * 0x9E37…9F4A_7C15) >> LEAF_CALL_RECORD_SHIFT`, indexing `ctx.leaf_records`;
3. **the reuse gate** — the record's `code_gen`, `epoch`, callee `hi` and `payload` all match → `hit`;
4. **on a miss** — split on `stable` (a stale epoch with `emit_leaf_state_at_rest` still true → restamp the epoch and reuse) else **the probe lane** (`leaf_call_probe`, which runs the interpreter's eligibility checks and *writes the record*);
5. **a hit** — `uses_env` → the env lane; else the in-frame lane: `entry == 0` → a stable rejection → `call_slow`; else the **aliased/fill split**, the **room check** (`argc*8 + frame_size*8 + stack_usage*8 <= ctx.buf_end`), then `entry(args_ptr, args_ptr + frame_size*8, ctx)` with `entry` the cached `LeafInlineInfo::entry`;
6. **the result tail** — the pending byte → `return_`; else restore the sp and push the result.
The **certified lane** (C1) is a separate miss target — the record's cached certified verdict, consumed by `call_slow`'s certified path.

**Why it is not separable.** The record the hit path reads is written *only* by the probe (step 4), so a partial "read the record + entry call" always misses until something runs the probe. The port therefore must include the probe lane — which is what makes it ~600 lines (`emit_call` 4689→~5000 + `emit_leaf_call_tail` + `emit_leaf_result_tail` + `emit_leaf_state_at_rest` + `emit_leaf_record_slot` + the certified lane).

**The port (`opt_lower.rs`), staged — each stage keeps `call_slow` as the fallback, so a partial port is always correct.**
- **G-a — the leaf in-frame lane.** `emit_leaf_record_slot` as a free fn; the `LeafCallRecord`/`LeafInlineInfo` field offsets (via `leaf_inline_offset`); the reuse gate; the hit's aliased path (the room check against `ctx.buf_end`, `entry(args_ptr, stack_ptr, ctx)`, the pending-byte tail); `call_slow` on every miss. New `Abi` field: the leaf-entry signature (`sig_entry`).
- **G-b — the probe lane.** Call the `leaf_call_probe`-equivalent helper (the per-step probe's helper, same args) so the record fills; plus the env lane and the `fill`/`not_aliased` split. **This is the stage that makes it pay** — without it, G-a always misses.
- **G-c — the certified lane + the self-call gate.**
- **G-d — the restamp** (`emit_leaf_state_at_rest`).

**The design question the port must answer.** The per-step lane is built on a running sp (`sp_var`) and a push-based value stack; the opt IR has neither (the operand stack is SSA values, and `abi.work` is the helper-arg region). So (i) the leaf frame's carve base is `abi.work` — a soft base, the `Op::Construct` precedent; and (ii) `Op::Call` yields the result as an SSA value, so the result tail is simpler than the per-step's — the pending-byte check then the value, with no sp restore or push.

**Acceptance.** The five `installed_jit_*leaf_call*` tests pass with the opt tier on (the F10 failure gone); `scratch/call_bench.js`-style parity (min-of-3); then re-run the F10 `Push` widening for its +10 rows; §6 gate.

**G probe results (2026-10-09).** Reading the leaf lane (`emit_call` 4689–~5000 + `emit_leaf_call_tail`/`emit_leaf_result_tail`/`emit_leaf_state_at_rest` + the certified lane + `emit_leaf_record_slot`) puts the port at **~600 lines, not ~300, and it is not separable**: the leaf record is written by the *probe*, which is part of the lane, so a "record read + in-frame call" partial still needs the probe and the result tails. Not started — a changeset that size on a `Lowerer`-state-dependent subsystem is a dedicated session, not a slice.

Two adjacent bounded attempts, both **inert and reverted**: (1) a counter *stack* in the lift (for nested/sequential fused loops) — `nested_loops`/`hof_methods`/`closure_capture` still bail `FastLoopHead`, so their head bails on the `var` case (a non-`Counter` head), not a stale counter; (2) `Op::RegExp` via `Step::RegExpLiteral` (the F6 step-index-helper pattern) — the single regexp row then bails on `Push` (its source string), i.e. the G-gated gap. **The F-arc lift coverage is now walled behind G (call parity) and the big feature ports** (for-of/for-in, switch/try/yield); every remaining bounded widening is inert.

### I5 — trial inlining, in full (opened 2026-10-08; probe-first)

Inline a monomorphic callee's `Step`s at a caller's call site, under a size budget, recursing. Targets (`optimizing-tier-plan.md` §6): `method_call` 6x, `js_call` 3.6x, `closure_capture`, `hof_methods`, `apply_call`. It is the **enabler of I6**: an allocation can be elided only where the code that allocates and the code that consumes it are one body, so escape analysis has nothing to work on until the call boundary is gone.

**The part we get for free.** SM allocates a fresh `ICScript` per call site so a function can be monomorphic *per caller* while polymorphic overall (`optimizing-tier-plan.md` §3.3). Slag's feedback record is keyed `(body, step)` and lives on the caller's `CompiledBody` — the body *is* the caller, so a site's record is already caller-private. Trial inlining needs no `ICScript` analog; the existing key is it (a `we delete` item, plan §3.7).

**I5a — the probe (no transform; land this first, and only this if it says no).**
- Extend `feedback.rs` with a `SiteRecord::Call(CallSite)` variant — the enum's documented seam — where `CallSite` holds the distinct callee identities it has seen (bounded by `MAX_OPTIMIZED_STUBS`, reusing `IcState`/`Observed`). The identity is the callee's **shared `CompiledBody` pointer**, not its function-box address: every closure from one declaration site shares one body (Cut 43), and re-instantiating a callee (a `bench()` that declares its callee in-body) mints a fresh box but keeps the same body. A box-address identity therefore reports a re-instantiated callee as polymorphic — an artifact of the bench protocol, not real call-site polymorphism — while the body identity is exactly what a trial inline guards on. A callee with no compiled body (a builtin) keeps its `Value` bits as identity and is never certified; a non-function is `0`.
- The writers: the call-step arms of `run_inner_inner` — `Call`, `CallFast`, `CallFastSlot`, `CallFastGlobal`, and the two fused `…Store` forms (the hot `x = f(args)` shape) — calling `CompiledBody::record_call(ip, id, certified, steps)`, mirroring `record_member_read`. The size is `CompiledBody::steps.len()` of the body the identity resolves to (when certified).
- Extend `feedback::summary` to classify call sites — monomorphic-within-budget / polymorphic / over-budget — and count callee-body step sizes.
- **The deciding number:** over the 77-row corpus and `--jit-bench`, the share of *call traffic* (weighted, not just site count) at sites that are monomorphic AND whose callee is a certified body under the budget, against the polymorphic count (the retirements a monomorphic inline would fire). A small share is a recorded negative, and I5 stops at I5a.
- Gate: §6's gate, no perf claim; the probe is off by default (`SLAG_FEEDBACK`).

**I5a result (2026-10-08): inlinable, decisively.** Over the 77-row corpus in the interpreter (`SLAG_FEEDBACK=1 … --jitless --corpus tools/corpus/workloads`): 97 call sites, **96.6M call observations, ~all monomorphic in code** (0 polymorphic, 0 generic), and **77.7M — 80% of call traffic — at a monomorphic site whose callee is a certified body ≤128 steps** (`inlinable_hits`). The non-inlinable fifth is builtin callees (`builtins` 4.8M all mono but none certified; `opcost` 12.9M mono / 4.7M inlinable, the rest `Math.*`/`Map.get`/array methods — I4's domain) plus a few non-certified callees (`control` 1.4M mono / 7 inlinable). The callees are small: of the 97 sites' first callees, 61 are ≤16 steps, 34 are 17–64, 2 are 65–256, none larger — so 128 is generous and 64 would already capture 95 of 97 sites.
- The named targets are 100% inlinable: `opcost/method_call.js`, `opcost/js_call.js` and `calls/direct_leaf.js` each show a single monomorphic site with a ≤16-step certified callee; `calls/` is 53.4M hits / **100% inlinable**, `language/` 20.0M / 95.8%.

So I5c is worth writing. The probe also fixes two things for I5c (they are I5c's work, not the probe's): the identity is a *body*, so inlining a closure's body needs its captured environment materialized (I5d); and a first cut keyed on one body retires if a site calls a second body — rare here (0 polymorphic observations), but the `ICState` valve stays for it.

**I5b — the premise and the retire hook (shared with I4/I6/I7).** An inlined callee's identity is a premise, and it is the first premise that is not a value-cell generation. A body carries the premises it was compiled under; invalidation retires the caller's body (`optimizing-tier-plan.md` §5). This is I3's remaining slice and lands here because inlining is the first transform that needs it.

**I5c — the transform, in full (design).** The splice itself is the easy half: at an `Op::Call` whose site is `Specialized` and whose callee is a certified body under the budget, replace the call with the callee's lifted IR — its parameters bound to the call's arguments, its return bound to the call's result — recursing under the budget. Three things ahead of it are the design.

- **I5c-0 — the lift has no call.** `emit_step` refuses every call step (`Unsupported::Step`), so no body containing a call enters the tier today; `Op::Call` exists in the IR with no producer and no lowering. I5c-0 lifts the call shapes a first cut needs — `Call`/`CallFast` (the callee is an operand-stack value) and the fused `CallFastGlobal`/`CallFastSlot` (the callee is a global cell / a frame slot) — into `Op::Call { callee, args }` and lowers it through the **existing** call helper on the identity path, so I5c-0 alone is behavior-neutral (more bodies enter the tier; the call is still a call). Two traps: the operand-stack effect is already in `max_stack_usage` (the call pops `this`+callee+args, pushes the result); and the arg-region protocol is not free — the interpreter's fast steps pass arguments by caller-frame base (`caller_arg_base`, Cut 35 slice 23) with `args_base_stack`, so the lowering must reproduce that region exactly or build the vector form, reusing the helper (§8: do not fork it).
- **I5c-1 — the identity, and how it is guarded (the load-bearing decision).** A call site's callee is a runtime value the IR does not statically know, so the inline owes a premise. The plan (§2) sanctions two mechanisms, and the call shape picks between them:
  - **A cell premise, retired.** `CallFastGlobal` is emitted only for a *never-assigned* global (the compiler's guard) and `CallFastSlot` only for a slot the compiler proved holds a certified closure, so those callees are stable by construction and need no runtime check: the premise is the cell's generation and a write **retires** the body (I5b) — the pure "retire, don't deopt" lane, and the cheapest.
  - **A computed callee, exited.** For `Call`/`CallFast` the callee is a stack value (a member read's result) the runtime does not track, so the inline carries `Op::GuardCallee(expected)`: a machine-code check of the callee against the one the site observed, exiting via `DISPATCH_DEOPT` on a miss (the §2 "exit, don't deopt" path, the same deopt `Op::GuardType` rides).

  **The guard is on the callee function *box*, not its body** — the box is what the compiled call path already carries (`callee: u64`), so the check is a value compare. But I5a measured monomorphism on the *body* pointer (the right identity for "is the code monomorphic"), which is a strict **superset**: a site whose box changes while the body stays fixed — a `bench()` that re-declares its callee, the corpus's shape — is body-monomorphic but not box-monomorphic, and a box guard misses there. So **I5a's 80% is the in-code ceiling**; the computed-callee lane's real coverage is the subset with a stable box, and **I5c's measurement must use hoisted-callee workloads** (the `tier-ab` shapes), not the re-instantiating `opcost` rows, or it will measure the corpus's artifact. The cell-premise lane carries no such caveat (a global/slot callee is stable in the workload too).
- **I5c-2 — the splice, and the invariant it must keep.** The inlined callee's frame slots become SSA values in the caller's IR (it is certified, so it has a frame; the call's arguments bind to its parameter slots). The plan's central constraint holds — the compiled frame must not diverge from the interpreter's — so the first cut inlines **only a guard-free callee** (`JitCompiledInfo::deopts` empty; the L2 invariant), because a resume across an *inlined* frame is work this cut does not do. The inlined body's heap references are SSA values invisible to the conservative stack scan, so they must be in `jit_roots` across a safepoint.

**Sub-cuts and order.** I5c-0 (lift + identity lowering, behavior-neutral) → I5c-1 (the cell-premise lane: `CallFastGlobal`/`CallFastSlot`/self, retired via I5b — I5b is its prerequisite) → I5c-2 (the computed-callee lane with `Op::GuardCallee`) → then I5d below.

**I5c-0 status (2026-10-08): landed.** The lift accepts `Step::CallFast` (the receiver and callee on the operand stack) into `Op::Call { args: [this, callee, a1..aN] }`, and `opt_lower` lowers it by materializing the arguments at the working-region base and calling the general `call_slow` helper through the pending-error ABI — no leaf-probe lane (a calling body is non-leaf), no new helper, no ABI fork. A calling body now enters the tier for the first time. The trap that first bit: the lift's depth fixpoint reads `stack_delta`, which mirrors `emit_step` and refuses any step it does not list — an `emit_step` arm without its `stack_delta` twin bails the whole body with a bare `Step("step")` (the same shape `step_name` reports for every unhandled step, so it reads as "some step" rather than "the call"). Still refused (the next slices): a direct-eval `CallFast`, `Call`, `CallFastGlobal`/`CallFastSlot` and their fused `…Store` forms, the tail calls and `Construct`. Gates: `cargo test --workspace` green (jit 319/0, the new `opt_tier_call_body_matches_the_interpreter` asserts the tier lowered an `Op::Call` via a test counter, not merely that some body lowered); `cargo clippy --workspace --all-targets -- -D warnings` clean; test262 `language` 23,726/0/0/0 and `built-ins` 23,820/0/1/0 at baseline.

**I5c-2 — the computed-callee lane, in full (design).** The splice needs the callee's IR *at compile time*, and the pass runs inside `JitEngine::compile`, which has the caller's `CompiledBody` (hence its feedback) but **no agent** — so the u64 identity I5a stores is not enough; the site must retain a **traceable handle to the callee's `CompiledBody`**. That handle, not the guard, is I5c-2's real prerequisite, and it makes the call-site record **always-on when the JIT is installed** (the probe's "a default build allocates nothing" is the probe's property, not the transform's). Three pieces:

- **I5c-2a — the handle.** The `CallSite` retains the first callee's **function id** (`Function::id()`, never reused) and its box, not an `Rc<CompiledBody>`. An id is the right handle because the body is *derived* state, rooted elsewhere — the leaf cache's precedent is `LeafEntry::trace`, which traces only the env because the body lives in `ecma_functions`. A retained `Rc` would either need a cycle-guarded trace (a body→body edge is cyclic under mutual recursion, and the tracer has no visited set) or keep a dead body's literals untraced; with an id the pass resolves `agent.ecma_functions[id]` and a stale handle is a **lookup miss**, not a dangling body. `Op::Call` gains its step index (`Imm::Int`), so the pass can find the site.
- **I5c-2b — `Op::GuardCallee`.** A `brif` that the callee `Value` equals the expected box (`Imm::U64`), deopting via `DISPATCH_DEOPT` on a miss (the same `emit_guard` `Op::GuardType` uses); the success path returns the callee. Landed first (inert until 2c produces it), mirroring T1's "the op and its lowering land before any producer".
- **I5c-2c — the pass (`pass/inline.rs`).** At an `Op::Call` whose site is monomorphic, its callee guard-free (`JitCompiledInfo::deopts` empty) and under the budget, emit `Op::GuardCallee` and splice the callee's lifted IR — arguments bound to its parameter slots, its return bound to the call's result — recursing under the budget.

The guard is on the **box** (the identity the compiled call path already carries), so I5a's body-based 80% is the in-code ceiling (I5c-1); measure on hoisted-callee workloads.

**I5c-2b status (2026-10-08): landed.** `Op::GuardCallee` and `Imm::U64` (a raw-bits `Const`, so a boxed `Value` — here the expected callee — is a constant) are in: the guard compares the callee `Value` (`args[0]`) against the expected (`args[1]`) and retires via the same deopt block `Op::GuardType` uses, yielding the callee on a match. `dce` keeps it, `cse` and `licm` leave it alone (a guard's timing is observable), and `default_effects` is `pure`. Inert until I5c-2c produces it — the T1 pattern (the op and its lowering land before any producer). A hand-built-IR test proves a match returns the callee and a mismatch returns `DISPATCH_DEOPT`, with no helper on either path. Gates: `cargo test --workspace` green (jit 320/0); `cargo clippy --workspace --all-targets -- -D warnings` clean; test262 `language` 23,726/0/0/0 and `built-ins` 23,820/0/1/0 at baseline.

**I5c-2a status (2026-10-08): landed.** `CallSite` records the first callee's function id and box (on `CallObserved::First`), the writers resolve them from the callee (`Vm::callee_call_info`), and `Op::Call` carries its step index (`Imm::Int`). **No GC edge is added** — the handle is a plain id resolved through the agent later — so `--jitless --gc-stress` records a row's 14M monomorphic hits with no crash and the tracer is unchanged. Inert until I5c-2c resolves the id. Gates: `cargo test --workspace` green (jit 320/0, runtime feedback 11/0 including the first-callee record); `cargo clippy --workspace --all-targets -- -D warnings` clean; test262 `language` 23,726/0/0/0 and `built-ins` 23,820/0/1/0 at baseline.

**I5c-2c — the splice, in full (design).** The splice replaces the call with the callee's IR. Two findings shape it.

1. **The callee's frame slots have no home.** The compiled frame *is* the interpreter's (sized by `scope`), and the inlined callee's locals are `FrameLoad`/`FrameStore(Imm::Slot)` into *its* frame, which the caller's frame does not contain — writing them would run off the frame. A rebased scratch region would diverge the compiled frame from the interpreter's (the plan's §2 invariant), so the fix is to **promote the callee's slots to SSA** (`FrameStore(s, v)` becomes a definition, `FrameLoad(s)` a use, with phis at joins — the shape the lift already builds for stack values). That promotion is its own slice.
2. **Retirement, not deopt, across the splice.** A guard *inside* the inlined region resumes at the *caller's call step*, re-running the call as a call; the inlined region's state is discarded, which is sound only if it holds no *frame* state the interpreter would expect (true once slots are promoted). So the first cut inlines only a **guard-free callee** and guards only before it.

Layering, chosen so the mechanics are agent-free and testable: `pass/inline.rs` takes `sites: &[Option<InlineSite>]` (indexed by step) and an injected `resolve: &mut dyn FnMut(u64) -> Option<Rc<Function>>`. The *site map* is built by `JitEngine::compile` from the caller's `feedback` (a plain u64 read — no agent); the *resolver* is the agent-dependent binding (`id` → a cached `lift` of `ecma_functions[id].ir`). The guard's live operand stack is the call's operands (`[this, callee, a1..aN]`), so a deopt at it re-runs the call step exactly.

Sub-cuts: **I5c-2c-i** the site map + the splice for a call that is a block's last instruction and whose callee is slot-free and call-free (the guard, the callee blocks cloned with fresh ids, each `Return(v)` → a jump to a continuation whose parameter is the result); **ii** the agent resolver; **iii** the slot promotion (mem2reg on the callee); **iv** recursion under the budget.

**I5c-2c-iii — the slot promotion, in full (design).** `ScopeInfo` fixes the callee frame's input layout: params occupy slots `0..arity` in source order, `this_slot` is a separate slot, `var`s follow, and a `let`/`const` slot is TDZ (the IR carries a `TdzCheck`). So a callee's slots split into two kinds and need two mechanisms:
- **iii-a, the input slots (no store).** A `FrameLoad(slot)` with `slot < arity` *is* the call's `i`-th argument, and `slot == this_slot` is the call's receiver — neither has a `FrameStore` in the body. Binding them means, in the clone, mapping the load's result to the call's operand (`args[0]` for `this`, `args[2 + slot]` for a param) and dropping the load. This makes a param-reading callee (`function (x) { return x + 1; }`, an accessor, an arrow) spliceable, and it needs the callee's `arity`/`this_slot` — so the resolver returns a `Callee { ir, arity, this_slot }`, not a bare `Rc<Function>`. The call must supply every param (`args.len() >= 2 + arity`) or the splice refuses (a missing argument is `undefined`, which the binding cannot synthesize).
- **iii-b, the `var` slots (stored).** A slot a body stores needs classic **mem2reg**: a forward dataflow whose merge of two live-in values is a new **block parameter** (the IR's phi), with the incoming edges passing their values. A slot whose loads are not all store-reached, or that is TDZ-checked, stays a frame slot (the pass then declines to splice the callee). This is the slice that makes a callee with locals (`function f(x) { var t = x * 2; return t + 1; }`) spliceable.

**I5c-2c-iii-a status (2026-10-08): landed.** The resolver returns `Callee { ir, arity, this_slot }`; the splice binds a callee `FrameLoad` of an input slot to the call's receiver or argument and drops the load. `spliceable` now admits a callee whose only slot accesses are input loads (a `FrameStore`, a `TdzCheck`, or a load of a non-input slot still refuses it — iii-b's territory). A hand-built-IR test proves a param-reading callee splices and computes (the bound value flows through), and a var-writing callee is still refused.

**I5c-2c-iii-b status (2026-10-09): landed.** `pass/mem2reg.rs` promotes a callee's storable `FrameStore`/`FrameLoad` slots to SSA — each store a definition, each load a use of the reaching definition, a merge of two defs a block parameter. Placement is the iterated dominance frontier of the store blocks (filtered to store-reached merges, never the entry block); the rewrite is a dominator-tree walk. Two guards keep it sound: a slot with a `TdzCheck` (a `let`/`const`) stays a frame slot, and every load must be **store-reached** (a load that can observe the initial `undefined` has no value to materialize) — either guard leaves a frame access and `spliceable` refuses, never a wrong program. The resolver runs it on the lifted callee and re-verifies, so a promotion bug is a refusal.

The one bug the slice hit was in the dominator test itself: `immediate_dominators` first tested the relation backwards ("d dominates o" where it must be "o dominates d"), which put a loop body under the entry instead of the header, so its loads were rewritten to the entry value. `verify` cannot see that — the graph is well-formed, only wrong — so the loop test asserts the body reads the header's phi; reverting the direction fails it. Measured (`scratch/inl_local_bench.js`, a `var`-heavy callee, 5M calls): 2.2s → 1.54s (~1.45x), identical output; the differential and `--gc-stress`/`--nursery-stress` runs (including a callee whose local holds an object across an allocation) are exact. Gates: `cargo test --workspace` green (jit 331/0, runtime 1043/0); clippy `-D warnings` clean; test262 `language` 23,726/0/0/0 and `built-ins` 23,820/0/1/0 at baseline.

**I5c-2c-iv status (2026-10-09): recursion under a budget, and the guard-free gate the earlier cuts always needed.** The splice now recurses: a `Callee` carries its own site map (`sites_from_feedback(&callee.body)`), and after cloning a callee the pass re-runs `try_inline` over its cloned blocks with the callee's sites, so a delegation chain (`a` → `o.b` → `o.leaf`) collapses into one body. A step budget (`SLAG_INLINE_BUDGET`, default 256, per top-level site) bounds the expansion and terminates a self-recursive callee — the call past the budget stays a real call. Measured (`scratch/inl_chain.js`, `a→b→leaf`): 0.25s → 0.20s (~1.23x), identical output; a self-recursive callee terminates and verifies.

The slice also closed a soundness hole the earlier cuts carried: `spliceable` accepted a guard-bearing callee, but a guard's deopt resumes its `imm` step — the *callee's* step, meaningless in the caller. It reproduces: a callee `function getx(o){ return o.x + 1; }` spliced into `call(o,p){ return o.m(p); }` throws `TypeError: undefined is not a function` the moment `o.x` is a string and the guard fires. `spliceable` now refuses `Op::GuardType`/`Op::Check` (the design's guard-free callee, I5c-1). Two consequences: the only guard in an expanded region is a splice's own `GuardCallee`, now emitted with the **outer** call's step and operands so a nested deopt re-runs the outermost call (sound because the region holds no frame state; confirmed by a mid-run nested-callee swap, `scratch/inl_deopt.js`); and the resolver must run the non-inline pipeline on the callee (`pass::optimize`), because a member-read callee's `GuardType` is pruned when its value feeds a call rather than arithmetic — without that, the guard-free gate would refuse every member-reading callee. A callee whose guard *is* consumed by arithmetic (the `getx` shape) stays refused: re-running the outer call would re-fire the getter the guard followed. Gates: `cargo test --workspace` green (jit 334/0, runtime 1043/0); clippy `-D warnings` clean; test262 `language` 23,726/0/0/0 and `built-ins` 23,820/0/1/0 at baseline.

**I5c-2c-ii — the resolver, in full (design).** The pass needs the callee's IR, and the runtime already has the agent on the TLS during a compile (`Agent::run_script_mode` wraps the whole run in `crux::function::with_agent`), so the resolution needs no plumbing: a runtime `resolve_callee(id)` reads `current_agent()` and returns the callee's `CompiledBody` plus its `scope.arity`/`this_slot`, and the jit resolver lifts it (cached per id). The finding that reshaped this slice is **soundness**: `Op::GuardCallee` puts the expected callee's box in machine code, and a box is recyclable once its function is collected — the leaf caches answer this by clearing their record on a sweep, which compiled code cannot do. So the splice must **root the callee**: the `CallSite` retains the callee as a GC `Value` (traced), and `CompiledBody::trace` traces the feedback. A callee kept alive by the caller's feedback never has its box recycled, so the guard is exact and this lane needs no retire hook. This amends I5c-2a, which chose a bare id to avoid a GC edge: too conservative, because the edge is a `Value` — a GC box, so the tracer's mark bits make it cycle-safe (unlike the `Rc<CompiledBody>` I5c-2a correctly rejected). The remaining work is the plumbing plus a `SLAG_INLINE` gate, so the splice goes live only when measured.

**I5c-2c-ii status (2026-10-09): the resolver landed; a probe-key off-by-one was the last blocker.** The plumbing is in: `runtime::jit::resolve_callee(id)` reads `current_agent()` (the TLS window `Agent::run_script_mode` opens around every run, so a compile is always inside one) and returns the callee's `CompiledBody` plus its `scope.arity`/`this_slot`; the jit resolver lifts it through `crate::opt::lift::lift`, cached per id, behind the `SLAG_INLINE` gate. The first end-to-end run still produced no splice, and the cause was not the resolver: the interpreter recorded the probe against `self.ip`, which the run loop has already incremented (`let step = &body.steps[self.ip]; self.ip += 1;` **before** the dispatch match), so every call/member-read record was filed one step **above** the step that the lift's `Op::Call` `Imm::Int` and `sites_from_feedback` key by. Recording at `self.ip - 1` fixes it; `feedback::tests::records_key_by_the_executing_step_not_the_next` pins the convention (mutating the recorder back to `self.ip` fails it). With the fix the splice fires (`JIT_DUMP_IR` shows `GuardCallee` and the inlined body) and the result is exact.

**I5c-2c-ii measurement (2026-10-09).** On a hoisted-callee block-last workload (`return o.m(x)` in a hot loop; `scratch/inl_hot.js`, 3M calls) the splice is ~1.65x on one binary: 0.45s → 0.27s, output identical. Net of the probe — `SLAG_FEEDBACK=1 SLAG_INLINE=1` against a plain default run — it is still ~1.6x (0.42s → 0.27s), so on this shape the probe pays for itself. The feature is **inert without both gates**: `SLAG_INLINE` alone resolves nothing, because `sites_from_feedback` reads the probe's store, which only `SLAG_FEEDBACK` allocates. So the splice stays off by default and the "enable by default" question belongs to the probe, not the splice — and it is deferred: the corpus A/B is too noisy to read here, the applicability is narrow (a call that is its block's last instruction, with a box-stable callee), and the honest next lever is mem2reg (I5c-2c-iii-b, below).

**I5c-2c-ii gates (2026-10-09).** The differential over a callee with a capture and `this` (both splice) and `arguments`, a tail call and a throw (all refused) is exact across `--jitless`, default, and `SLAG_FEEDBACK=1 SLAG_INLINE=1`; the spliced run is `--gc-stress` and `--nursery-stress` clean (the retained callee `Value` keeps the guard's box alive). `cargo test --workspace` green (jit 324/0, runtime 1043/0); `cargo clippy --workspace --all-targets -- -D warnings` clean; test262 `language` 23,726/0/0/0 and `built-ins` 23,820/0/1/0 at baseline.

**I5c-2c-i status (2026-10-08): landed.** `pass/inline.rs` splices a monomorphic site's callee (when the call is the block's last instruction and the resolved callee's IR has no `FrameLoad`/`FrameStore` and no `Op::Call`) — the guard, the block clone, the `Return` → continuation — behind an injected resolver and a site map. `JitEngine::compile` builds the site map from `body.feedback` and resolves through a stub for now, so a default build is byte-identical: with the probe off (or the callee off-budget) the pass emits nothing. The splice mechanics are proven on hand-built IR.

**Gate.** §6's gate, plus: the call rows fall (`--jit-bench` `non-leaf call` jit ms, the `tier-ab` call rows — never the load-sensitive ratio, §8); a differential over a callee with captures, `this`, `arguments`, a tail call and a throw; the deopt/retire path pinned (a site whose callee changes mid-run exits or retires and produces the interpreter's answer); and the retirement count small — I5a's 0 polymorphic observations predict no thrash, but the count is the check.

**Open.** Whether the computed-callee lane guards the box or pays for a *body*-identity helper (a body guard survives re-instantiation and would cover the corpus's shape, at a helper cost); and whether the first cut's premise keys on the callee alone or also the argument count.

**I5d — widening.** Recursion beyond one level, then the callee shapes the leaf lane already admits (captures, `this`, `arguments`).

**Traps.** §8's list applies in full; two consequences are I5-specific. Inlining grows a body past `JIT_MAX_COMPILE_STEPS` (128 debug / 1024 release), and an over-cap body silently returns to the interpreter, so measure sizes as inlining lands. And the inlined callee's heap references become SSA values, invisible to the conservative stack scan, so they must be in `jit_roots` across a safepoint. Retire thrash — the design's central risk — is what I5a's polymorphic count measures before I5c ships.

**Open (settle as I5 lands).** The inlined callee's IR source (cache the lifted `Function` per certified body, or re-lift per site) and the size budget's number (start from I5a's step-size histogram, then SM/V8's ~30 B "small"); the premise-keying question sits above, in I5c.

### I6 — escape analysis + scalar replacement (design)

**Goal.** An object or array a wholly-compiled body creates and does not let escape becomes SSA values: its fields (or constant elements) are plain `ValueId`s and the reads/writes over them fold. The allocation, its shape work and its GC pressure all disappear. Targets (`optimizing-tier-plan.md` §6, stage E): `object_alloc` 20x, `array_alloc` 20x, `array_slice` 33x, then `typed_array_for_each` 46x, `construct_churn`, `destructure`, `json_*`, and `object_keys` 234x if its keys array can be built without leaving the body.

**It is non-speculative, and that is the whole design.** §2's invariant — the compiled frame mirrors the interpreter's, so there is no side table and no `Recover_*` — holds only while every compiled body is a *renaming* of the interpreter's execution. Escape analysis is the first transform that can break that: an object the code gave up on is a thing the interpreter would have had on the heap. So the analysis is **exact, not speculative**: it elides an allocation only when the object provably never escapes the body, and the "provably" is a static property of the lifted IR (dominance plus the effect sets), never a runtime guess. A partial escape is handled by *materializing* the object at the escape point — an explicit `NewObject` inserted on the escaping path — not by a guard that could fail after the fact. Where the analysis cannot prove non-escape it declines and the allocation stays: a missed optimization, never a wrong program.

**The prerequisite: allocation must lift.** Today the lift refuses every literal step — `ObjectFast`/`ObjectBegin`/`ObjectInitName`/`ObjectInitComputed` and `ArrayFast`/`ArrayBegin`/`ArrayElement`/`ArrayEnd` all fall to `Unsupported::Step` — so a body that creates an object or array **does not enter the tier at all** (`object_alloc`, `array_alloc` and `object_keys` are all still per-step rows). `Op::NewObject`/`Op::NewArray` are declared in `ir.rs` and never produced or lowered. I6-0 is therefore not an optimization but the ground it stands on: lower the literal steps into `NewObject`/`NewArray` + `Op::MemberStore`/`Op::ElementStore`, and lower those two ops in `opt_lower` (reusing the existing object/array helpers — no new semantics). Behavior-neutral, the same "the op and its lowering land before any producer" discipline I5c-2b used; the corpus and the sweeps pin the equivalence. (The coverage probe below qualifies the order: I6-0 is the *first* blocker for 32 rows, but every corpus row's loop is fused — `BuilderBind` — so I2's fused-`for` lift is the necessary unblock before I6-0 pays.)

**The analysis (I6a).** A *virtual allocation* is an `Op::NewObject`/`Op::NewArray` whose result flows only through operations the analysis models. Two things make it exact:
- **The escape lattice.** A virtual object escapes when it reaches an operation that can retain it: an argument to `Op::Call`/`Op::Construct`, a value stored by `Op::MemberStore`/`Op::ElementStore`/`Op::GlobalStore` into a receiver that is *not* itself elided, a `Term::Return`/`Term::Throw` operand, or the environment a `Op::NewClosure` captures. Every read (`Op::MemberCellLoad`, `Op::MemberGuard`, `Op::ElementLoad`) and the arithmetic/comparison set does not escape it. The effect sets already carry the distinction — an op whose `Effects::call()` can retain its operand escapes it; a read that `may_read(Heap::Slots|Elements)` does not — so the walk is a fixpoint over the SSA graph with escape-set propagation (an object stored into another object is as escaping as that one). The `clobberize`-style oracle I7 needs is the same piece.
- **A field is replaced only when it does not escape either.** The elided value is a tuple of per-field SSA values; a field holding a heap reference re-opens the §8 rooting trap, so the first cut admits **scalar fields only** (Numbers, booleans) — exactly the `object_alloc` (`({ a: 1 }).a`) and `array_alloc` (`[k].length`, `[1, 2, 3]`) shapes. A heap-valued field (`object_keys`' array of strings) needs `jit_roots` and is I6d.

**The rewrite (I6a).** For each non-escaping virtual allocation, drop the `NewObject`/`NewArray` and keep a per-object `(key | index) -> ValueId` map:
- `Op::MemberStore(obj, key, v)` with `obj` elided defines `map[key] = v`; a `MemberCellLoad`/`MemberGuard` on `obj` with a mapped key becomes the value (the cell probe folds away — a virtual object has no shape to probe). A read of an unwritten field must respect the `has`/`delete` semantics the body observes, so the first cut proves the field written on every path (mem2reg's store-reach rule, reused) and declines otherwise.
- `Op::ElementStore(arr, i, v)` with a **constant** `i` and `arr` elided defines `map[i] = v`; a constant `ElementLoad` folds to it, and `arr.length` folds to the high-water index. A computed index declines (a phi'd tuple of elements is its own slice).

**The trap that bounds the first cut.** A virtual object's fields are SSA values, and §8 says SSA values are not on the conservative stack scan. With **scalar** fields this is moot — a Number is not a root. The moment a field holds a heap reference (a string, a nested object) the lowering must spill every live such value into `jit_roots` across a safepoint (an allocation or a call), or materialize the object before it. That is I6d, and it is the same `jit_roots` mechanism a spliced body's heap references need (I5c's trap).

**Interaction with I5.** Escape analysis only pays where the allocator and every consumer are one body, and I5 is what makes them one. `object_alloc` becomes elidable once a `bench()` that declares its object and reads it in the same body lifts; `construct_churn` once the `Construct` inline (I5c-1's cell lane) puts the constructor and its `new` in one body. The order is deliberate — I6 without I5 would elide almost nothing.

**Sub-cuts and order.**
- **I6-0** — lift + lower the allocation forms (behavior-neutral; the corpus is the gate). Unblocks every row.
- **I6a** — the escape lattice + scalar replacement for a fully non-escaping object/array with scalar fields and constant indices. Targets `object_alloc`, `array_alloc`.
- **I6b** — the iteration/result scaffolds (`array_slice`, `typed_array_for_each`'s callback result), once I5's `.forEach` inline lands.
- **I6c** — partial escape: materialize at the escape point (the "sink"), so an object returned after a fully-replaced pre-escape life still loses its pre-escape allocation.
- **I6d** — heap-valued fields + `jit_roots` rooting (shares I5c's mechanism); reaches `object_keys` if the keys array can be built within the body.

**Probe and gate.** Probe the allocation count: `--gc-trace` per corpus row (plus the `--gc-stress` counts) before and after, and the `--jit-bench` allocation rows (`object_alloc`, `array_alloc`, `array_length_write`) jit ms. Gate is §6's, plus: the alloc rows fall, **no row regresses** on the jit-loss gate, and `--gc-stress`/`--nursery-stress` are clean (the elided object's fields are the new roots the analysis must not have dropped). A negative — the analysis declines the corpus's shapes — is recorded and I6 stops at I6-0.

**Traps.** §8's list applies; three are I6-specific. The escape propagation must run to a **fixpoint**, not one pass (an object stored into another elided object is still non-escaping until that one escapes). A virtual object read with a key that is not provably the same property (a computed key, a proxy, a symbol) must decline — the object's observable shape is the whole point. And an elision that grows a body past `JIT_MAX_COMPILE_STEPS` returns it to the interpreter silently, so measure sizes as it lands.

### The lift-coverage probe (2026-10-09) — the fused `for` loop, not allocation, is the corpus-wide blocker

A probe before the I6-0 lever (`scratch/probe_steps.sh`: run every corpus row, name the step each bailed body tripped on, aggregate the *first* blocker per row) reframes the priority. The first blocker across the 77 rows:

| first blocker | rows | family |
|---|---|---|
| `ArrayFast`/`ArrayBegin`/`ObjectFast`/`ObjectBegin` | 32 | allocation literals (I6-0) |
| `BuilderBind { slot: None }` | 18 | the fused `for` loop (I2) |
| `ArgsBase` | 12 | the argument-vector call form |
| `FunctionDeclInit` | 7 | nested function declarations |
| `LoadContextSlot`/`InitContextSlot`/`CreateFunction` | 4 | the capture context |
| `HoistGlobalGuard` | 2 | global reads |
| `SwitchDisc`, `JumpIfTrueKeep` | 4 | switch / for-of heads |

The first blocker understates the fused-loop gap: **all 77 rows use a `for (` loop** (3 use `while`). A canonical `for (var i = 0; i < n; i++) { s = s + i; }` body bails on `BuilderBind { slot: None }` — the fused-loop prologue — while the same loop written `while` **lifts** (an IR dump, against no dump and a bail for the `for`). `BuilderBind{None}` is a no-op in the interpreter (`if let Some(slot) = slot`), but it is only the first of a chain the lift refuses: `FastLoopBind`, the fused `JumpIfLtImm`, `RunRegBody`, `FastLoopHead`, `FastLoopStore`. So the literal-blocked rows do not lift with I6-0 alone: `array_at` (`[1, 2, 3, 4].at(2)`), which bails on `ArrayFast` first, bails on `BuilderBind` once the literal is hoisted above the loop — its loop is fused.

So the sequence inverts the plan's assumption. **The fused `for` loop (I2's remaining part) is the necessary unblock for every corpus row**, and I6-0 (allocation) is second — still worth landing as the first blocker for 32 rows and behavior-neutral, but it unblocks a row only where the loop is not fused, which the corpus almost never is. The fix is not an optimization but a reshaping: the fused steps are a counter loop the IR can already express (`while` proves it), so the work is to model `RunRegBody`'s register body and the `FastLoop*` counter as SSA. `BuilderBind`/`BuilderStore` with `slot: None` are no-ops. (But the reshaping is blocked on opt-path parity — try it and the element-read tests catch the regression, below.)

**The fused-loop lift is blocked on opt-path parity (measured 2026-10-09).** An attempt to lift the fused canonical loop — `BuilderBind`/`BuilderStore`/`FastLoopBind`/`FastLoopStore` as no-ops, `FastLoopHead` as a terminator over the counter's binding slot, `Step::PushAcc`/`PopAcc`/`IncAcc`/`DecAcc` as frame accesses, and `RunRegBody`'s numeric `LeafOp` subset — does lift the canonical body (`for (var i = 0; i < n; i++) { s += i; }`), but it is a **regression**, and the test suite says so: `installed_jit_dense_array_element_read_inlines` and `installed_jit_typed_array_element_read_inlines` fail (`for (var i = 0; i < n; i++) { s += a[i & 255]; }`). Both bodies previously bailed to the per-step path, whose `emit_element_read` inlines the dense/typed element read; the opt path's `Op::ElementLoad` lowers through the `get_member_computed` helper, so a lifted body loses the inline read — and the tests count the helper calls and force them under 100. The lift was reverted.

The lesson is structural and generalizes past the fused loop: **widening the lift is gated on the opt path being at least as fast as the per-step path for every shape it lifts**, or entering the tier is a pessimization. The per-step path has accumulated inlining the opt path has not — the dense/typed element read is the one the tests pin — and every new liftable shape re-opens the comparison. So the order is: port the per-step fast paths the opt path lacks (the inline element read first, then `CallIntrinsic`), *then* widen the lift. Landing the fused-loop lift first would trade the corpus's bails for slower compiled bodies.

**The dense element read is ported (parity port 1, landed 2026-10-09).** `opt_lower`'s `Op::ElementLoad` no longer lowers through `Helper::GetMemberComputed`; it inlines the dense-Array read (`opt_lower::emit_element_read`) with gates identical to the per-step `emit_dense_element_read_into` — the packed Object-tag compare, `array_dense != null` as the dense switch, the canonical-index round-trip plus the `< 2^32-1` bound, `idx < elem_len`, and the hole decline to the helper. Every other receiver, key, or a hole still declines to the helper (the typed-array arm and the G8 computed-read cell are not ported yet — correct, just not inlined). Measured (`scratch/read_bench.js`, a `while` body — the shape the tier lifts, 5M iterations × 9, min-of-3, one binary): the opt path **61ms with the helper → 31ms inlined (~2x)**; the per-step path (`SLAG_OPT=0`) is 66-68ms, so the port takes the tier from a wash against per-step to ~2.2x faster. That closes the field the fused-loop attempt tripped on for the dense case; the typed-array arm is the next parity port before the lift can widen. Gates: `cargo test -p jit --lib` 335/0; `cargo test --workspace` green; `cargo clippy --workspace --all-targets -- -D warnings` clean; test262 `language` 23,726/0/0/0 and `built-ins` 23,820/0/1/0; the `--gc-stress`, `--gc-verify` and `--nursery-stress` differentials identical.

**The typed-array read is ported (parity port 2, landed 2026-10-09).** `opt_lower` now dispatches the dense arm (`emit_dense_element_read`) and the numeric-TypedArray arm (`emit_typed_element_read`, with `emit_typed_element_number`/`emit_typed_element_float`) off the same `typed_array`/`array_dense` cursor test, with the read gate (not detached, not resizable, non-null data, `idx < array_length`) and the per-kind conversion chain. The lane keeps the per-step single-agent rule: under `workers` it declines straight to the helper. It is **performance-neutral on the micro** (`scratch/ta_read_bench.js`, a `while` body over a monomorphic `Uint8Array`, 5M × 9, min-of-3): the pre-typed-port opt binary is ~81ms, the post-port ~80ms — the `get_member_computed` helper is already cheap for a monomorphic typed read (the same finding as the reverted member-cell probe). The port is still required: it makes the opt path inline-identical to the per-step path, so widening the lift cannot make a lifted typed-read body take the helper and trip `installed_jit_typed_array_element_read_inlines`. Gates: `cargo test -p jit --lib` 336/0 (the new `installed_jit_lifted_typed_array_element_read_inlines` uses a `while` body); the workspace, clippy and sweep gates unchanged; `scratch/ta_read_port.js` identical under `--jitless`/`--gc-stress`/`--gc-verify`/`--nursery-stress`. Next: `CallIntrinsic`, then re-open the fused-loop lift.

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
