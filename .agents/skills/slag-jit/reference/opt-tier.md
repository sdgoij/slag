# slag-jit — the optimizing tier

Guards, splices, lifting, LICM, the GC safe point, nested runs.

Part of the `slag-jit` skill; [`../SKILL.md`](../SKILL.md) is the index and
carries the gates. Open the file a trap points at, not all of them.

## 22. The optimizing tier's speculation guard (T1) — `vm.ip` is not per-step

The optimizing tier's guards (`Op::Check`, `Op::GuardType`) lower through
`opt_lower::emit_guard`: on failure it mirrors the live operand stack into the
working region (`Abi::work`), sets `ctx.suspend_sp` and `vm.ip` to the resume
step, and returns `DISPATCH_DEOPT`; `run_jit_body` rebuilds `vm.stack` from the
region and re-enters the body at `vm.ip`.

**Trap: `vm.ip` is NOT maintained per compiled step.** It is written only at a
try-exit and at a deopt probe — during normal compiled execution it holds a
stale value. No compiled code or helper may treat `vm.ip` as "the current
step"; it is meaningful only after a deopt has returned to the runtime.

- **The `vm.ip` write is null-guarded** (`ctx.vm == null` skips it), matching
  `emit_completion_store`, so the bare-ctx test scaffold (`run`) can execute a
  guard's deopt with a null `vm`. In production `run_jit_body` always passes a
  real `vm`, so the store happens.
- **A new guard op must be excluded from every pass.** `Op::Check` and
  `Op::GuardType` are `pure` by `default_effects` yet are explicitly kept by
  `dce`, skipped by `cse`, and refused by `licm::hoistable` — a guard's timing is
  observable (dropping or moving one changes when the body deopts).
- **`Op::GuardType` types its result value.** Its `Inst.ty` is the guarded type;
  a dominated op reads `types[operand]` to pick the tag-free path
  (`emit_bare_numeric`), so the guard result must be a *new* value, not the
  operand re-used. T1 implements `Type::Number` only (`is_double`); any other
  guarded type is `Unsupported` (the per-step path), never a silent guess.

## 23. Trial-inline splices must stay guard-free (I5c-2c-iv)

A guard's deopt resumes its `imm` step with a mirrored operand stack (section 22).
That `imm` is a step index in the body the guard was **lifted from**, so a callee
inlined into another body would carry deopt steps that are meaningless (and
wrong) in the caller. `spliceable` therefore refuses any callee holding
`Op::GuardType`/`Op::Check`.

- **The failure mode is a live crash, not a wrong number.** A callee
  `function getx(o) { return o.x + 1; }` spliced into a caller throws
  `TypeError: undefined is not a function` once `o.x` is a string and the guard
  fires: the caller resumes at the callee's step and re-runs the wrong step.
  `cargo test` and the sweeps will not catch it — the sweeps run with
  `SLAG_INLINE` off.
- **The resolver must run `pass::optimize` (fold/narrow/guard/cse/licm/dce) on
  the lifted callee, not just `mem2reg`.** A member-read callee's `GuardType` is
  pruned when its value feeds a call rather than arithmetic; without the guard
  pass on the callee, the guard-free gate would refuse *every* member-reading
  callee (a delegation chain `a → o.b → o.leaf` would never splice).
- **A nested splice's own `GuardCallee` must resume the OUTERMOST call** with its
  operands (`try_inline`'s `root`), not the nested call: the inlined region holds
  no frame state, so discarding it and re-running the outer call as a call is the
  only sound deopt target. A callee whose guard *is* consumed by arithmetic stays
  refused — re-running the outer call would re-fire the getter the guard followed
  (the read a `GuardType` trails may be a getter).

## 24. The lift-coverage census is a stack-delta count, not the real lift rate

`blocking_step_names` (`crates/jit/src/opt/lift/mod.rs`) enumerates blockers by
`stack_delta(step).is_err()` with terminators excluded. It therefore sees a step
only when the lift's *stack-delta model* refuses it — never when `emit_step`
refuses a *variant* of a step the model accepts. The fused-loop gate is exactly
this shape: `step_name` collapses the whole fused family to `"FusedLoop"`, and
`emit_step` accepts only the accumulator-path head plus the `None` builder/num
slots, refusing `FastLoopHead` with a `Slot`/`Global` counter and the `Some`
(Route-B `num` / string-builder) slots. `stack_delta` accepts all of those, so
the census reports such rows as "fully lifting" when the real lift `opt bail`s.

- **Never read the census as coverage.** A family dropping to 0 in
  `opt-blockers` means its steps now pass `stack_delta`, not that any body lifts.
  Measure the real rate by counting the IR actually produced: `JIT_DUMP_IR=1`
  prints one `--- IR ---` per accepted lift, `JIT_DUMP_CLIF=1` prints
  `opt bail: {reason}` per refusal (`scratch/probe_liftrate.sh` counts both).
  As of 2026-10-09 that rate is 11 of 77 corpus rows — far below the census's 48.
- **The first `opt bail` reason names the real blocker.** `opt bail:
  Step("FusedLoop")` is the `Some`-slot gate (a `Some`-slot `BuilderBind`/
  `BuilderStore`/`FastLoopBind`/`FastLoopStore`); `Step("FastLoopHead")` is the
  non-`Counter` head. Those two, not the argument-vector family, gate the
  canonical `for` loops and almost every arithmetic corpus row.
- **Assert on the per-cache opt count, not the global `OPT_COMPILED` delta.**
  `OPT_COMPILED` is process-global and the tests run in parallel, so any
  concurrently-lifting test supplies the delta and a test whose own body never
  lifted still passes — four `installed_jit_lifted_*` tests were vacuous this
  way until 2026-10-09. `JitEngine::compile_counted` + `JitCache::
  opt_compiled_count` (reached through `with_opt_jit_agent_counts`) give a
  per-cache count immune to that pollution. A test for a lift feature must also
  use a body the lift actually accepts (a straight-line body called past
  `JIT_COMPILE_THRESHOLD`, or the exact acc-path loop shape) — verify with
  `cargo test -p jit --lib <name> -- --test-threads=1`.

## 26. Lifting for-of/for-in: the fetch is a divergent terminator

The opt tier lifts the fused-bind `for-of`/`for-in` shapes: `Op::ForOfBegin`/
`ForOfNextBindLocal`/`ForOfClose`/`ForInBegin`/`ForInNext`/`ForOfBindLocal`
(all `Effects::call()`), plus `opt_lower::emit_for_of_bind` (the G17 fast-array
cursor port) and `licm`'s slot-store accounting. `ForOfNext` (the non-fused
stack form) is refused (`is_for_fetch` names it so the bail is explicit).

- **A fetch's two targets are `back` and `done`, NOT `[jump_target, end]`.** The
  prologue fetch's `back` IS its fall-through (`back = step_index + 1`), so the
  generic conditional shape collapses both edges onto one block. `starts` and
  `block_successors` special-case `for_fetch_targets`.
- **The bottom fetch's dead `ForOfClose` tail.** A `break`-less loop's
  `ForOfClose` (at `end_label`) is reachable only via a `break`, but it sits
  right after the bottom fetch, so `end - 1` is not the fetch. `terminator_index`
  finds the block's FIRST terminator, `starts[i+1]` is not marked for a fetch,
  and the dead tail is dropped.
- **A helper's raw `0`/`1` code is not a `Type::Bool` value.** The IR's
  `Term::Branch` lowering, for a `Type::Bool` condition, compares against the
  NaN-boxed `false` bits — a raw `0`/`1` fetch code always branched true (the
  done edge was never taken; a re-entry fetch then hit an empty for-of stack:
  `ForOfNext without a for-of`). `raw_code_to_bool` converts it to a canonical
  `Value` boolean.
- **`licm::slot_stores` must name EVERY frame-slot writer, not just
  `Op::FrameStore`.** `ForOfNextBindLocal`/`ForOfBindLocal` write their `imm`'s
  slot directly, so a loop's element `FrameLoad` was hoisted out of the body
  (`s` accumulated the first element only). `Op::FunctionDecl`'s slot is only in
  the step payload, so it flags `slots_unknown` and blocks all slot-load
  hoisting — an under-accounting here is a silent wrong value, never a refusal.
- **The for-in key is the fetch's value, with `undefined` as the done sentinel.**
  The IR's operand stack is SSA and cannot push-then-pop across the branch, so
  `Op::ForInNext` returns the key or `undefined` (a for-in key is always a
  string), and the terminator branches on `!= undefined` and passes the key to
  the bind block as an edge argument.
- **The `!= undefined` sentinel comparison must inline; by default it lowered to
  two helpers per element.** `lower_inst` mapped `Op::StrictEq` to `BinarySlow`
  and `Op::Not` to `UnarySlow`, so an opt for-in loop paid THREE helper calls
  per element (`for_in_next` plus the two) where the per-step pays one plus an
  inline `icmp`/`brif` — measured **2.2x** on an isolated for-in body. Both are
  now inline: `x === undefined` (either order) is a bits compare against the
  unique sentinel (sound — `undefined` is not a Number, so no `+0`/`-0`/NaN
  aliasing applies), and `!b` on a `Type::Bool` is a canonical-bits flip.

## 27. The non-`Counter` fused head needs a Number fast path

A fused canonical loop's head is `Counter` (the acc-path, Number-proven) or a
frame-slot `Slot` (`FastLoopVar::Slot`); `emit_fused_test` accepted only
`Counter` until 2026-10-09. The `Slot` head's semantics are `fast_loop_inc` — a
Number increments as `num ± 1`, anything else via `update_value` (ToNumeric,
BigInt-aware) — so it is a generic `++` plus a generic relational.

- **`Op::UpdateValue`'s lowering MUST inline the Number case.** It lowers
  through `update_value_slow`, but a plain helper call per iteration regressed
  `nested_loops` to 44.9ms vs the per-step's 29.3ms (~1.5x slower) — the head
  is the loop's whole cost and the per-step inlines `num ± 1`. The lowering
  branches on `is_double`, does an inline `f64` add with `canon_double`, and
  falls to the helper only for a non-Number. This is the read-parity contract:
  measure the head-dominated row (`nested_loops`), not a method-call-dominated
  one (`hof_methods`, ~parity either way).
- **A `Global` head is still refused.** Its `++` needs the identifier-update
  machinery (`update_ident`), a separate helper and name resolution.

## 28. The computed-read element read, and a const's type must match its value

`opt_lower::emit_element_read` (the `Op::ElementLoad` lowering, fed by
`Step::GetMemberComputed` and the register `LeafOp::GetMemberComputedLocal`)
ports the per-step emitter's three arms in order: the dense-Array element read,
the numeric-TypedArray read, and the G8 computed-read cell. The cell is the
key-identity probe (`JitCallContext.computed_read_cells`, its slot =
`runtime::ir::computed_read_cell_index` — the emitter and the runtime MUST
compute the identical slot, a divergence is silent) mapping a String key's
identity to its interned atom, then the `member_value_cells` value validated by
`(id, name, generation)`. Without the cell arm a lifted `o[k]`/`a[k]` falls to a
`get_member_computed` helper per iteration — the pessimization that had
`GetMemberComputedLocal` reverted once already.

- **A `Const`'s declared `Type` must match the value it holds; a mismatch is a
  silent optimization loss after CSE.** The lift emitted three `undefined`
  constants (the `this` for `CallFastSlot`/`CallFastGlobal`, and
  `emit_reg_body`'s accumulator seed) with `Type::Unknown`. CSE is block-local
  and merges a later same-op/same-imm value into an earlier one, so a same-block
  `Type::Undefined` for-in sentinel was merged into an `Unknown` `undefined`
  const and LOST its type — and anything keying on a value's static type (the
  the `=== undefined` inline above, `narrow`'s proofs) then silently degrades. Emit
  every constant with its exact type.

## 29. The LICM hoist guard is a real probe, not a free branch

`crates/runtime` compiles a certified fused loop whose body has loop-invariant
member/global reads into a `Step::HoistMemberGuard`/`HoistGlobalGuard` head plus
TWO copies of the loop: a GUARDED copy whose reads are hoisted into hidden frame
slots (the guard's hit path), and the general per-iteration copy (the miss
path). The opt lift must not model the guard as "always miss": that is a
*pessimization*, not a neutral choice — the per-step tier takes the hit and
reads once, while the opt tier then runs every read per iteration.

- Measured: `objects/own_read` (`s += o.a + o.b`) was **+571%** in the opt tier
  (19.8 vs 3.0ms) under the always-miss model, and the `globals/*` read rows
  ~+60% each. The lift now REFUSES the guard (`Err(Unsupported::Step)` in
  `emit_fused_test`) so those bodies run the per-step tier, which implements it.
- The refusal costs opt coverage (69 → 63 IR-producing rows) because the guarded
  copy already lifts (its reads lower to hoist-slot `FrameLoad`s) — only the
  guard's probe and hoist-slot stores are unmodelled. Implementing the guard in
  the lift is the follow-up that recovers both the coverage and the hoist.

## 31. A body's read guard must not cost it the leaf-call lane

`lift` emits `guard_read_value` — a `GuardType` asserting a read's value is a
Number so the arithmetic consuming it lowers tag-free — and any body carrying a
`GuardType` or `Check` is marked `deopts`. **Every machine-code inline lane
refuses a `deopts` callee** (`leaf_call_probe`, `certified_verdict`, the
self-call path), because the caller's compiled code has no deopt path. So a
*leaf* body that carries the read guard can never be inlined and its call falls
to `call_slow` — measured as `calls/construct_churn` (a prototype method called
on a fresh object) being 1.5x slower in the opt tier than the per-step tier,
whose bodies are not `deopts`.

- The fix: `lift` gates the guard on `!body.leaf`. A leaf body is a call-site
  inline candidate, so its inlinability is worth more than its own tag-free
  arithmetic; a non-leaf body, which no caller inlines, keeps the guard.
  Removing a speculation is always sound — the arithmetic falls back to its
  tag-checked path.
- Diagnose with `JIT_LEAF_TRACE=1` (the rejection reason — `deopts` here) and
  confirm with `JIT_HELPER_STATS=1` (`LeafCallFill` up, `CallSlow` to 0).

## 32. The int-op fast path's redundant work

`call_int_binary` (the inline path for `&`/`|`/`^`/`<<`/`>>`/`>>>`) is the whole
cost of an `opcost`-shaped loop (`k = i & MASK; s = (s + k) | 0`), ~24
instructions per bit op. Three pieces were free to remove:

- **The result needs no `canon_double`.** It is an integer `f64` (`fcvt` of an
  i32), and an integer's bits never land in the NaN-boxed tag region, so the
  canonicalization was three no-op instructions.
- **`is_double` is unnecessary for an operand the typer proves a Number** —
  mirror `call_numeric_binary`'s `known` pair. That needs `narrow`'s
  `is_numeric_inst` to include the bit ops (a bit op on Numbers is a Number; the
  arg-numeric guard is required because a BigInt operand yields a BigInt).
- **The `|x| < 2^63` range guard is elided for an operand proven an int32
  (2026-10-10).** `narrow` carries an `Int` lattice beside the `Number` one
  (`slot_int`/`value_ints`/`is_int_inst`/`is_int_const`), the lift's `Type::Int`
  on a `&`/`| 0` result reaches the lowering through the value table, and
  `trunc_i32(…, known_int)` returns the constant `1` guard when set, so
  `call_int_binary` skips the `fabs`/`fcmp`; a `(bool, bool)` int pair beside the
  `known` pair feeds it, and the `known && known` tag conjunction constant-folds.
- **Only `Const`, an `Int`-slot `FrameLoad`, and a bit op on numeric operands
  produce `Int` — an `Add` does NOT.** A sum of int32s can exceed `2^31`, so
  `narrow` types it `Number`. A counter stored `i = i + 1` therefore stays `num`
  and its `i & MASK` left operand keeps the guard; only `k = i & MASK` (a
  bit-op result), `s` (stored a bit-op result) and the constants are provably
  `Int`. Do not "fix" this by typing `Add` `Int` — it is unsound.
- **Keep `is_numeric_inst` (the `Number` lattice) alongside `is_int_inst`.**
  They are independent greatest fixpoints sharing `slot_flags`; replacing the
  former with the latter (a natural-looking edit) leaves `value_numerics`
  calling a function that no longer exists.

## 33. The optimizing tier must emit the GC safe point

`compiler.rs::emit_gc_probe` (the per-step loop-header poll) had no `opt_lower.rs`
twin until 2026-10-10, so **every loop that lifted into the optimizing tier ran
with NO GC probe**: it allocated in machine code without `gc_safepoint` ever
firing, the young cohort ballooned (1,503,182 boxes measured on a 300k-iteration
string loop, which made 0 safepoint calls), and the eventual collection was a
~46 ms pause. It is process-lifetime visible — a workload's JIT time depended on
what ran before it (`strings/search_slice` 60 -> 1739 ms in the whole-corpus
process) while `--jitless` was unaffected. Full write-up:
`.notes/archive/perf/corpus-regression.md`.

- **A new lowered-loop form needs its safe point.** The probe is emitted in
  `opt_lower::lower` at each loop header — a block targeted by a back edge
  (`target <= source`, which covers a self-branch). A terminator or loop shape
  that is not a back-edge target gets no probe.
- **The probe splits the header block; the IR parameters must flow through it.**
  `emit_gc_probe` creates a `poll`/`cont` diamond; `cont` carries the block's
  parameters (passed from the header) and the loop body runs there, so the
  parameter binding happens after the probe. The helper call goes through
  `call_helper`, whose pending check is the termination channel (an error bails
  the body).
- **"A compiled loop never collects" points here first.** The tell is a lifted
  allocating loop with no GC activity: `--gc-trace` shows collections far below
  the allocation volume, and the `gc_safepoint` helper's count (the
  `JIT_HELPER_STATS` histogram, indexed by helper) sits at zero. The regression
  test `installed_jit_opt_loop_safe_point_runs_minors` pins it (a lifted
  allocating loop runs minors and its cohort stays bounded).
- **A whole-corpus measurement is the only place this shows.** An isolated row
  is fine (its cohort is collected at the script boundary); the pathology needs a
  preceding allocating workload. `node tools/corpus/bench.js` at the README's
  commit vs HEAD is the check.

## 34. A nested run on the caller's Vm must restore every per-activation Vm field

The certified-callee lane (`run_certified_call_nested` → `run_compiled_body_on`)
runs the callee **on the caller's `Vm`**. `run_inner` recomputes
`Vm::globals_unshadowed` at every activation, and `VmScratch` is the contract
that puts the caller's state back. It did not list that field, so a callee whose
chain was shadowed (or that read no names — `global_reads_are_unshadowed`
returns `false` for an empty list) left the caller's flag `false` for the rest
of its run.

The failure is an internal `SyntaxError: StoreIdent without a resolution` at
the first global assignment after such a call: `ResolveIdentStore` skipped the
reference push while the chain looked clean, then `StoreIdent` re-read the
clobbered flag, took the dynamic path and popped an empty `var_ref_stack`.
Goats reproduced it in `terrainMesh = rl.makeModel(...)`.

- **The compiled tier hides it.** `LoadIdent`/`StoreIdent` in machine code read
  the per-call `JitCallContext`, so a compiled caller is unaffected; an
  *interpreted* caller reads the shared `Vm` field. `--jitless` and a warm JIT
  therefore diverge, and a trace showing `ctx_clean=true` next to
  `vm_clean=false` is the tell.
- **Every field `run_inner` writes is a candidate.** When adding one, check
  whether `save_scratch`/`restore_scratch` list it. The regression test is
  `ir::tests::a_nested_run_restores_the_callers_globals_unshadowed`.
- **The store sites are defensive now.** An empty reference stack falls back to
  `store_ident_value(..., true)` (the global binding the pre-resolve targeted)
  instead of throwing, in both the interpreter and `store_ident`.
