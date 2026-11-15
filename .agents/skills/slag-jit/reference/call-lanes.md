# slag-jit — call lanes

The leaf cache, the compile threshold, certification, the self/certified-callee lanes.

Part of the `slag-jit` skill; [`../SKILL.md`](../SKILL.md) is the index and
carries the gates. Open the file a trap points at, not all of them.

## 16. Leaf-cache revalidation on a stale epoch (Cut 68)

The per-callee leaf verdict (`LeafCallRecord` in the agent's
`Agent::leaf_records`, addressed through `JitCallContext::leaf_records`) is
only trusted while `record.epoch == ctx.leaf_epoch` and
`record.code_gen == ctx.leaf_gen`. A "disturbing" helper
(a getter, `valueOf`/`toString`, a nested call) bumps `leaf_epoch`, so the
next visit misses. Before Cut 68 the miss ALWAYS re-probed — a monomorphic
hot call next to a getter probed every iteration (100K probes per loop).
Now the compiled gate re-validates the eligibility state and reuses the
verdict when it is at rest. Traps:

- **The epoch must gate separately from identity.** `hit = (identity_ok &
  gen_ok & epoch_ok)`; the re-validation runs only when the callee AND
  generation still match (`stable`) — a stale epoch on a DIFFERENT callee
  must probe, never re-stamp. The re-stamp path (`stale_block` →
  `emit_leaf_state_at_rest()` → ok? store `record.epoch = live epoch` →
  jump to the HIT block) reuses the cached verdict INCLUDING a cached
  rejection (entry 0 → `call_slow`). The ONE exception is a *deferred*
  rejection: a body the compile threshold has not promoted yet leaves
  `ir.jit_info` at `0`, and the probe clears the record's identity for it (see
  §17's promotion chain) so the record re-probes and inlines once the body
  compiles. A *sticky* refusal (`jit_info == 1`: over the step cap, or an
  emitter refusal) is reused as before.
- **`emit_leaf_state_at_rest` must mirror `leaf_call_probe`'s checks** —
  all seven `Vm::can_inline_leaf` control stacks empty (`try_stack`,
  `pending`, `for_of_stack`, `for_of_boundaries`, `for_in_stack`,
  `async_for_of_stack`, `destructure_stack`), `env_stack.len() == 1`,
  `realm_count == 1`. If it ever drifts from `can_inline_leaf`, the
  revalidation can wrongly reuse a verdict across a real state change.
- **The `Vec` len field is private** — `offset_of!(Vec, len)` won't
  compile. The offsets are `pub const`s in `runtime/jit.rs` computed as
  `2 * size_of::<usize>()` (the structural invariant: `Vec<T>` is
  ptr + cap + len) for the private fields; `EnvStack.len` was made
  `pub(crate)` so `offset_of!` works for it in the same crate. The jit
  crate cannot name `pub(crate)` `Vm`/`EnvStack`, hence the constants.
- **The `bint`/`band` borrow conflict**: `let empty_64 = self.bint(empty);
  ok = self.builder.ins().band(ok, empty_64);` — passing the `bint`
  result inline into `band` borrows `self.builder` twice and fails.
  Always hoist the `bint` into a temp first.
- **The record is keyed by the CALLEE, in the agent's table (G15).** A record
  holds the leaf's entry + frame descriptor, and both are a pure function of
  the callee's compiled body — so the table is keyed by the callee alone and
  one record serves every site that calls it. The table lives on the `Agent`
  (`Agent::leaf_records`, addressed through `JitCallContext::leaf_records`)
  rather than inline in the per-run context: a context is created once per JIT
  run INCLUDING the hot leaf path, and an inline table made every run pay its
  memset (a 64-entry ctx table took `calls/recursive_fib` 329 → 732 ms before
  the move). The slot is `runtime::jit::leaf_record_slot(callee)` — a
  golden-ratio multiply taking the HIGH bits — mirrored by the compiler's
  `emit_leaf_record_slot`, kept in lockstep by
  `the_emitter_and_runtime_leaf_record_slots_agree` (a divergence is SILENT:
  the verdict is written to one slot and read from another, so the site never
  inlines). Use the high bits: the payload is the box address >> 4, so
  consecutive closures differ only in the low bits, and the low-bit fold this
  replaced (`>> 4` / `>> 16` / `>> 28`, or `payload & mask`) collapsed a run
  of 64 closures onto a handful of slots. A collision is only a re-probe (the
  exact identity + `code_gen` still gate reuse) — never a correctness issue,
  and never a place to "fix" by loosening the check. A fresh run starts cold
  for every slot: `LeafCallRecord::empty()`'s `callee_hi = u32::MAX` is
  impossible for a real value.
- **The probe-count drop is the evidence, not wall time.** The getter's
  own call cost dominates, so A/B wall-time deltas were noise (0.025 vs
  0.024s); the defensible measurements are the probe counts 100K → 1
  (stale-epoch revalidation) and ~200K → 2 (two hot sites) — the
  `installed_jit_stale_epoch_leaf_cache_revalidates_at_rest` and
  `installed_jit_two_hot_leaf_sites_each_cache_separately` e2e tests wrap
  `leaf_call_probe` in a counting fn via
  `JitHelpers.leaf_call_probe` and assert the flat counts across a
  100K-iteration loop.
- **The probe takes the call's unbound receiver (G12).** `leaf_call_probe(ctx,
  callee, this, args, argc, slot)` — the extra `this` is the spec's
  `thisArgument`, and the probe applies `OrdinaryCallBindThis` before filling the
  frame's `this` slot: strict as-is; sloppy an object as-is, a nullish one → the
  realm's global object, a primitive one *boxed* (it allocates and can throw, so
  that case still refuses and stays on `call_slow`). The distinction that makes a
  method inlineable: a `this` read in a certified body lowers to
  `Step::LoadLocal { this_slot }` (a frame read, leaf-eligible) — `Step::ThisValue`
  is leaf-excluded and only appears for `super`/receiver contexts, so a body whose
  `this` is a plain read IS `leaf_inline`-certified and the old
  `scope.this_slot.is_some()` refusal was the only blocker. A body that reads
  `this` *and* an identifier is not certified at all (`LoadIdent` is leaf-excluded),
  which is a `leaf_lookup` miss rather than a `this`-slot refusal when you trace
  it. Measured on the corpus's `calls/method_call`: `CallSlow` 14,000,000 → 8,
  132.7 → 77.9 ms.
- **A non-aliased hit rebuilds the frame from the RECORD, not the scope
  (G14).** `emit_call`'s cache-hit path calls a leaf in-frame only when the
  frame IS the argument region (`frame_size == arity` with all args present);
  every other frame (a `this` slot, or any `var`/lexical slot past the params)
  goes to `leaf_call_fill(ctx, callee, this, args, argc, slot)` — note
  `sig_call_slow` (the callee selects the record slot; there is no `site`
  argument any more). The probe caches the whole fill
  descriptor in the record (`LeafInlineInfo.this_slot`/`strict`/`tdz_mask`/
  `fill_ok`) and the hit path rebuilds from it alone, returning the cached
  entry or 0 (the frame no longer fits → `call_slow`). Measured payoff: the
  plan's shape (2M calls of `function f(x){var t=1;return x+t}`) 35.4 →
  **21.4 ms**, and `calls/method_call` 77.9 → **58.7 ms** — that row is
  non-aliased, so it was the real cap on G12. Do NOT reintroduce a `leaf_lookup`
  here: a floor build that returned the entry right after the cache check
  showed the lookup + the `Rc` clone + the scope deref were ~7 ns of the
  per-call cost, while the indirect call itself is under 1 ns. `leaf_call_fill`
  is registered in all four mirror files and is deliberately NOT in
  `disturbs_leaf_eligibility` (it is a read plus a frame write, no re-entry, no
  compile).
- **Both paths fill through one `fill_leaf_frame(&LeafInlineInfo, &TdzSource,
  …)`.** The probe passes `TdzSource::Store(&scope.tdz_store)` (exact for any
  frame); the hit path passes `TdzSource::Mask(info.tdz_mask)` (exact for
  `frame_size <= 64`). A frame wider than the mask records `fill_ok = 0` and
  the emitter's `not_aliased` block routes it back to `probe_block` — a
  truncated descriptor must never be used to fill, or the lexical slots lose
  their TDZ marker silently. If you add a field to `LeafInlineInfo`, keep the
  hit path's loads going through `leaf_inline_offset`, and keep
  `leaf_fill_info` (probe) and `leaf_call_fill` (hit) reading the SAME fields.
- **`OrdinaryCallBindThis`'s global comes from `JitCallContext::global_bits`,
  not the agent.** The fill needs the realm global's BITS for a sloppy nullish
  receiver; snapshotting them once per call (next to `global_object`, from the
  same `vm.global_object(agent)`) keeps the fill off the `Agent` borrow — which
  is what let the hit path drop the `Rc` clone — and makes the probe and the
  hit path bind the identical global. Every `JitCallContext { … }` literal
  (three in `ir.rs`/`jit.rs`, four in the jit crate's tests) needs the field.
- **An environment-using leaf gets the env lane, not `call_slow` (G13).** A
  body reading a captured binding lowers to `LoadContextSlot`, which is
  leaf-safe, so it IS `leaf_inline` — but the machine code cannot call it
  in-frame: the `body_context`/`lexical_env` swap has to span the call
  (including its error exit) and be undone before the caller resumes. The probe
  records `uses_env` and still refuses (so the site's first call takes
  `call_slow`, which runs the leaf via `try_jit_leaf`), and the hit path's env
  lane calls `leaf_call_fill` (frame + room check, the same helper the in-frame
  lane uses) then `leaf_call_env` (lookup the env → `new_body_context` → swap →
  the compiled entry on the CALLER's ctx and buffer → restore), returning the
  value or `u64::MAX` to fall back. Mirror `run_jit_leaf`'s env handling AND its
  tail order exactly — the restore must happen before the machine code's pending
  check — and note `new_body_context` is always `None` for a leaf, because a
  leaf cannot create closures and so captures nothing itself. Measured on a
  monomorphic env leaf: 92 → 40.5 ms at 2M calls. A frame wider than the
  record's TDZ mask records `entry = 0`, so the site falls back instead of
  filling from a truncated mask.
- **A polymorphic call site needs one record per callee (G15).** The record
  holds ONE callee identity, so a site that sees N callees used to re-probe on
  nearly every visit: `calls/closure_capture` (64 closures through
  `fns[i & 63](i)`) showed `LeafCallProbe`/`CallSlow` 7,000,000 for 1,000,000
  iterations — every visit took the probe and then `call_slow` — and a floor
  build showed the probe's own body was only ~1.4-3.5 ns/call, so the cost was
  the verdict never holding. Keying the agent table by the callee fixed it: the
  same row now shows `LeafCallProbe`/`CallSlow` ~900 (the warm-up only) with
  `leaf_call_fill`/`leaf_call_env` 7,000,000 (the env lane on every visit), and
  the row fell 88.8 → **50.9 ms**. Because a record's `entry` outlives a run,
  it must also carry the code GENERATION (`LeafCallRecord::code_gen` against
  `JitCallContext::leaf_gen`): a run bumps `Agent::leaf_gen` before its first
  `lookup_info` — whose compile may EVICT, freeing the code a previous run's
  record named — so a stale record re-probes instead of jumping to a freed
  entry. Nested runs share the enclosing run's generation (no eviction can
  happen while a frame is in flight). A sweeping collection still clears the
  table (`gc_safepoint`), because a recycled box address could match a record's
  identity. `LEAF_CACHE` (the interpreter's per-function leaf cache) was raised
  16 → 256 for the same reason: `leaf_call_env` re-derives the closure env
  through it on every call, so a table smaller than the site's callee count
  sends each call to the `ecma_functions` HashMap.
- **Helpers are reached through an UNTYPED function pointer — arity slips are
  silent.** `emit_raw_call` calls a helper's ADDRESS; a call site left on the
  old argument list compiles cleanly and the machine code, not the compiler,
  decides what the helper reads. That already cost a debugging cycle (a lane
  left passing 4 values to a 5-value helper silently fell back to `call_slow`).
  After changing any helper's arity, grep every `emit_raw_call`/`call_slow`
  for it — the compiler will not tell you.
- **Read the probe count before believing "the leaf lane refuses this body".**
  A per-visit probe count usually means the SITE holds no verdict
  (polymorphic, or a slot collision), not that the body is ineligible: G13's
  audit found `calls/closure_capture` was polymorphic, not env-refused.
- **The hit path's cache loads MUST wrap `leaf_inline_offset`.** The probe
  path's base (`info`) already points at `leaf_inline`, so it uses a bare
  `offset_of!(LeafInlineInfo, …)`; the hit path's `cache` points at the
  `LeafCallRecord` record, so every field load there must go through
  `leaf_inline_offset(offset_of!(…))`. Dropping the wrapper on ONE field
  (e.g. `arity`) reads the high half of `callee_payload` instead, which makes
  `aliased` false almost always: the aliased fast path goes dead and every
  hit takes the fill path, producing silently wrong values (not a crash).
  This is exactly the kind of plausible-looking wrong answer the sweeps
  catch and unit reasoning does not — `JIT_DUMP_CLIF=1` shows it as the load
  offset in the cache gate.

## 17. The compile threshold (Cut 69) — the gate in `lookup_info`

`lookup_info` (`runtime/src/jit.rs`) is the single choke point all four JIT
decision sites call (`run_jit_body`, `run_jit_resume`, `run_jit_leaf` via
`try_jit_leaf`, and `leaf_call_probe`). Since Cut 69 it gates
STRAIGHT-LINE bodies behind a consult counter:

```rust
// in lookup_info, replacing the known==0 branch:
if ir.jit_calls.get() < JIT_COMPILE_THRESHOLD && !ir.has_loop {
    ir.jit_calls.set(ir.jit_calls.get().saturating_add(1));
    return std::ptr::null();   // NOT cached: the next consult re-counts
}
let ptr = hook.lookup(...);    // compile
```

Traps that cost debugging time:

- **The `body_has_loop` ↔ `step_targets` sync.** `body_has_loop`
  (`runtime/src/ir.rs`, `pub(crate)`) mirrors the JIT compiler's
  `step_targets` (compiler.rs): any step whose jump targets include an
  index `< its own` is a back edge; PLUS the implicit-loop steps
  `FastLoopHead`/`RunRegBody`. When you add a step with jump targets,
  update BOTH `step_targets` (the compiler's back-target seeding) and the
  ir.rs mirror — the runtime owns the `Step` enum, so the mirror lives
  there. A skew only misclassifies the loop heuristic, never semantics.
- **Self-tail-call steps are loops.** `TailCallSelf*` are in `body_has_loop`
  even though their `step_targets` are empty: the interpreter's TCO loop
  (`run_inner_impl` re-entering the body on `VmOutcome::TailCall`)
  NEVER re-consults `lookup_info`, so a pure consult count could never
  promote a recursive chain — the plan's "promotes after K calls"
  premise is false for every TCO shape. The general tail-call steps
  (`TailCall`/`TailCallFast*`) are NOT loops (a computed callee like
  `getF()(n-1)` is not statically self-recursive), so a tco-call-args
  style body stays interpreted — only its per-step closure (consulted per
  iteration through the ordinary call) promotes.
- **The `call_slow → try_jit_leaf → run_jit_leaf` promotion chain is
  load-bearing.** A hot leaf under a compiled caller: the site's probe
  caches a REJECTION (entry 0, below threshold), the machine code routes
  to `call_slow` → `do_call_fast` → `fast_call_core` → `try_jit_leaf` →
  `run_jit_leaf` → `lookup_info` — count++ per call. The counter reaches
  K even though the site never re-probes, and after promotion the leaf's
  machine code runs; the caller's site *re-probes* on its next visit (the
  probe clears the identity for a deferred rejection — §16) and then inlines.
  Before that clearing the site cached the below-threshold refusal as permanent,
  so the caller stayed on `call_slow` for the whole run and only a *fresh*
  caller (a new call, or a later benchmark invocation) saw the inline path.
- **The count aggregates only across `Rc` clones of ONE declaration
  site.** Function declarations and arrows share the compiled body per
  site (Cut 43); a `function` expression VALUE does not — each
  repetition in a test like `f({ f: function (x) { ... } }); f({ f: ...
  });` is a DIFFERENT AST node, hence a fresh body whose count stays at
  1. E2e tests that need a body promoted must call it against ONE
  hoisted site (`var o = { f: fn }; f(o); ×17`), and a stateful closure
  must be re-fresh per call (`make()(); ×17`, not `f(); f();` on one
  closure).
- **The threshold path never writes `jit_info == 1`.** The tri-state
  (`0` unknown / `1` sticky-unsupported / `>1` compiled) is preserved:
  below-threshold bodies stay at `0` (each consult re-counts), and the
  sticky mark is only written at/after the threshold, so promotion is
  never blocked. A body consulted ≥K times that the hook rejects still
  sticks at `1`.
- **Eviction stays correct**: a cleared `jit_info` re-consults with
  `jit_calls ≥ K` (or `has_loop`), so it recompiles immediately.
- **Straight-line scripts never compile** (consulted once per
  `eval_program`), so the compiled completion-register path is only
  exercised by LOOP scripts in e2e — the
  `installed_jit_script_completion_matches_the_interpreter` table's
  straight-line cases assert `min_compiled == 0` and verify behavior
  parity only.
- **One-shot async/generator bodies stay interpreted** (resumed once per
  `yield`/`await` — a 2-await body gets 3 resumes), so the compiled
  suspension path is exercised by loop-containing generators/async
  bodies; the e2e tests that need a resumable body promoted drive it
  ≥17 times (a script loop calling the async fn 17×, or 6 generator
  instances).

## 18. The certification path: scope gate vs emit_step gate

The compile decision has TWO independent gates, and the failure mode tells
you which one tripped (verified 2026-09-01 on the `--bench` micro rows):

- **The scope gate runs BEFORE the JIT is ever consulted.** `run_jit_body`
  is reached only from `run_compiled_body`, which `ordinary_call` takes
  only when `ir.scope.is_some()` (function.rs) — plus the leaf paths
  (`run_jit_leaf`/`try_jit_leaf`). A body with `scope = None` (the
  env-machinery path) NEVER reaches `lookup_info`: the cache never sees
  it, `jit_info` stays 0, and there is no sticky "1" mark. Symptom: a
  body that doesn't speed up AND produces no cache lookup at all is a
  scope-certification failure, not an `emit_step` failure.
- **The emit_step gate.** A body with `scope.is_some()` that reaches the
  JIT bails when any step has no `emit_step` arm (or needs a missing
  helper). `lookup_info` marks it sticky-1 ("known non-compilable"), so
  the next call skips the cache too.

The gates are independent: a body can pass scope and fail emit_step
(`Destructure`-era examples aside, the live one is any step this skill has not
seen an arm for), or fail scope and never be seen at all.

**`Step::Construct` IS lowered (corrected 2026-09-27).** This section
previously claimed the opposite, and the claim was stale: `emit_step` has a
`Step::Construct { .. }` arm that calls `Helper::Construct` (the
construct-inline leaf cache / general path), `max_stack_usage` accounts for it
(net 0), and `step_name` answers `"Construct"` — so a body containing `new`
compiles, and the catch-all would NOT report `"unsupported step"` for it.
It cost a plan entry: `.notes/archive/perf/non-leaf-jit.md` §6 named `Construct` as the next
milestone on this section's word, and the step that actually bailed deno's last
offered body was **`TypeofIdent`** (a `typeof x` over a `BindingLoc::Env`
name; lowered as Cut 93). **Lesson: verify a "not lowered" claim against
`compiler.rs` before planning around it** — the `emit_step` arms and the
`step_name` entries are the only authority. A grep for `Step::<Variant>` across
`crates/jit/src/compiler.rs` settles one variant in a command; enumerating the
`Step` enum and diffing it against `compiler.rs` settles all of them at once,
which is how the real gap was found in minutes after the wrong one had already
been planned around.

**The catch-all name is only as good as `step_name`'s coverage.** When
adding a step arm, add its `step_name` entry in the same change — handled
steps without an entry (member reads, Push/Pop, FastLoopHead, ...) would
print the fallback if their arm ever moved, and a genuinely un-lowered
step should identify itself.

**Compiling a body is not the same as making it faster.** The JIT leaf
call (`run_jit_leaf`: probe + `jit_roots` buffer + `JitCallContext` setup
+ machine-code call) can cost MORE than the interpreter's in-place
`run_inline_leaf` for a tiny (≤4-step) leaf. Measured: a 1M-iteration
`n = f(n)` loop with a small captured arrow (`(y) => x + y`, a 4-step
leaf) compiled fine and ran ~25% SLOWER than the interpreter (41.6 vs
33ms on the closure-capture row) — the per-iteration leaf-call overhead
dominated the compiled loop's gains. Judge JIT wins by shape, not by "it
compiled"; the interpreter's leaf-inline dispatch is a fast baseline for
micro-leaves. The per-iteration machinery
(`EnterPerIteration`/`PerIteration`/`UpdatePerIteration`/`CreateArrow`)
IS lowered; loops whose hot call targets a dynamic callee (`fns[j & 15]()`)
stay ~par because `call_slow` dominates regardless of compilation.

**Investigation recipe:** temporarily print in `JitCache::lookup` (first
visit: step count, `has_loop`, `scope`, `leaf`) and in `JitEngine::compile`
(the `Unsupported` error). No first-visit print for the body → scope gate;
a print + bail → emit_step gate, and the error names the step.

## 20. The compiled self-call (G21) — `call_slow`'s runtime fast path

A compiled `CallSlow` site whose callee IS the running closure is run by
`call_slow`'s `self_call_inline` (`crates/runtime/src/jit.rs`) on the shared
ctx, with a nested frame in a private per-call buffer — no re-entry through
`do_call_fast`/`run_compiled_body`/`run_jit_body`. The runtime path is
runtime-only (no new helper); since Phase 2a an emit-side gate reaches it
directly without the leaf-record probe (§20.1). It is gated in
`CompiledBody::self_call_eligible`
(`crates/runtime/src/ir.rs`) and passed to `run_jit_body` as `self_call_ok`
(true only from `run_compiled_body`, false from the script/async/generator
drivers, or a resumable body would take it and return a plain value instead of
a promise/generator). Traps:

- **The nested run SHARES the caller's ctx, so two caller-owned cursors must be
  swapped to the nested buffer and restored: `ctx.buf_end`** (a leaf call inside
  the nested body re-checks its room against it — leaving it pointing at the
  caller's buffer makes the nested leaf carve into the caller's frame) **and
  `Vm::array_index_stack`** (a throw mid-literal would otherwise leak an entry
  into the caller's next literal). The private buffer is rooted via
  `vm.jit_roots` for the call's duration (the Vm is already an active-run root);
  `agent.jit_depth` is bumped so the cache cannot evict mid-run.
- **The gate is what makes the shared environment sound.** It excludes every
  body that builds a fresh per-call env (`context_names` non-empty), reads
  `this`/`arguments`, or carries try/for-in/for-of/async-for-of/destructuring/
  suspension/`NewTarget`/`CreateArguments`/`CallApply`. Reading the *enclosing*
  context (`LoadContext`, as `fib` does for its own name) is fine — the env is
  the same object at every recursion level — but a body that would call
  `new_body_context` is not: the closure's outer env is not on the ctx.
- **The path pushes no `ExecutionContext`** (same as the leaf-inline and
  `TailCallSelf` paths), so a stack trace loses self-recursive frames. That is
  deliberate and consistent; do not "fix" it by pushing a context per level.
- **`ctx.self_entry`/`self_stack_usage`/`self_inline_ok` are set at run entry**
  (from the cache `info`), which is why `run_jit_body` needs the `self_call_ok`
  parameter and every `JitCallContext` literal (4 in `crates`, 4 more in
  `crates/jit/src/lib.rs`'s scaffold tests) must set the three fields. A new
  literal that forgets them fails to compile — which is the point.

`installed_jit_a_self_recursive_call_matches_the_interpreter` (differential +
absolute answers; mutation-check by zeroing the param fill) and
`installed_jit_a_deep_self_recursion_falls_back_past_the_depth_cap` (the
`MAX_JIT_DEPTH` decline and the catchable guard) pin it.

### 20.1 The emit-side gate (Phase 2a)

`emit_call` (`crates/jit/src/compiler.rs`) now loads
`JitCallContext::current_function` and branches straight to the `call_slow`
lane when the resolved callee's full 64 bits equal it (plus a non-zero check so
a script's `0` never matches), skipping the leaf-record gate. This is sound
because the running body contains this very call step, so the leaf probe could
only *refuse* the callee. `slow`/`merge` are hoisted above the record loads, and
the gate is emitted only when `self.scope.is_some()` (a script has no scope and
`current_function` 0). The compare+branch is always not-taken at a non-self
site, so it costs below measurement; `slow` gains one extra predecessor, which
is fine because blocks seal at `seal_all_blocks`.

### 20.2 Two traps this arc measured

- **`--jit-bench`'s ratios are `jit/interp`, and the interpreter column is
  load-sensitive — read the raw *jit* ms, not the ratio.** The `non-leaf call`
  row (`bench` -> `mid` -> `leaf`) read 0.52 then 0.40 then 0.51 across three
  sessions at an unchanged ~13.8 ms jit; only its `interp` moved (26.9 vs
  34.9 ms). A ratio-only read attributed a self-call win to a row that has no
  self-call in it.
- **A `[Value; N]` stack buffer is initialized to `undefined` before it is
  rooted, for a reason — do not shrink `N` to save the init.** Bounding the
  self path's buffer at 24 slots measured ~11% on `recursive_fib`, but every
  slot the GC traces must hold a *valid* Value: a persistent frame arena cannot
  skip the init, because a reused slot holding a dead box's bits traces a stale
  `GcAny` (its vtable/payload walk is unsound), and a smaller *static* array
  heap-spills for any `frame_size + stack_usage > N - JIT_STACK_SLACK`, i.e. a
  malloc per recursive call. Measure both directions before touching it.

### 20.3 The shared-state hazard (the Phase 1 bug)

**A nested self run shares the caller's Vm AND ctx. Any step that mutates
caller-owned Vm state, or runs user code while building it, must be gated out of
`self_call_eligible` — the self path does not resume the machinery those steps
drive.** The bug that proved it: a self-eligible body whose base case is a
*strict* tail call emits `TailCall*`, whose helper runs `tail_prepare_ordinary`
(swapping `vm`'s frame/context for the tail callee) and sets `ctx.tail`;
`self_call_inline` ignored both, so the outer body resumed on a Vm reset for
another body and pushed the tail helper's placeholder — `10714` where the
interpreter answers `11200`. The gate now excludes every `TailCall*`/`TailCallSelf*`
variant, `ArgsBase`/`ArgsPush`/`ArgsSpread`, the vector `Call`, `Construct`,
`TaggedTemplate`/`TailTaggedTemplate` and `SuperCall`. Two live traps for the
next widening (2b, the general call):

- **A non-self callee CAN run on the caller's Vm/ctx — but only through the
  certified-callee lane's full discipline (§21).** Sharing is what produced the
  tail bug, so the earlier reading here was "do not share"; Cut 1 showed the
  tractable shape is to share the `Vm`/ctx *with* explicit scratch isolation, a
  private frame buffer, and an env-correct resolution path. Read §21 before
  forming an opinion.
- **`Vm::args` is live across `ArgsSpread` (it iterates a user iterator),** so a
  nested run that clobbers it corrupts the outer argument build — the reason the
  vector-call steps are gated, not just the `Call` step itself.

## 21. The certified-callee lane (Stage 9 Cut 1) — a different body on the caller's Vm

`call_slow` gained a second fast path beside `self_call_inline`:
`certified_call_inline` resolves the callee's `EcmaFunction` record, requires
`CompiledBody::certified_callee_eligible()`, and runs the callee's compiled
entry with a nested frame in a private buffer and its own `JitCallContext` —
skipping `do_call_fast`/`ordinary_call`/`run_compiled_body`/`run_jit_body`, the
pooled Vm take/reset, and the `ExecutionContext` push. A miss (ineligible,
uncompiled, below threshold, sticky-refused) falls to the funnel, which ports
and promotes. Measured 2.7× on `--jit-bench`'s `non-leaf call`; the widenings
below took the corpus `mean-jitGap` 78.3 → 70-71. The traps, all of which
produced a wrong answer first:

- **A method callee needs `OrdinaryCallBindThis` into its frame's `this` slot,
  and the super / `ThisValue` machinery must be refused.** The lane's gate
  (`certified_callee_eligible`) is the self gate **minus** the `this_slot`
  exclusion — the lane binds `this` (strict: as-is; sloppy nullish → the callee
  realm's global object; sloppy object/function: as-is; **sloppy primitive:
  falls back**, because boxing allocates and can throw) — **plus** the steps
  that read the *running context's* `this` binding or home object: every
  `super` step (`GetSuperName`/`GetSuperComputed`/`GetSuperComputedKeep`/
  `GetSuperBase`/`AssignSuper*`/`ResolveSuperRef*`/`UpdateSuper*`/`DeleteSuper`/
  `SuperCall`) and `ThisValue`, whose reader `Vm::vm_this_binding` walks the
  current `ExecutionContext` a lane run does not install. A plain `this` read in
  a certified body is a frame-slot load (`LoadLocal { this_slot }`), not
  `ThisValue`, so it stays eligible. `self_call_eligible` keeps excluding
  `this_slot` — a self-call's receiver can differ from the running one.
- **`arguments` bodies: the unmapped (strict) form only, and only same-realm.**
  The lane sets `Vm::call_args` for the run via `mem::replace`, **only when
  `scope.arguments_slot.is_some()`** — so an ordinary call stays
  allocation-free (a `to_vec` `args` slice). `run_leaf_body` has the same swap.
  Two refusals stay: the **mapped** (sloppy) form, whose per-name accessors
  alias the callee's *capture context* which the lane does not build (and whose
  names make `context_names` non-empty), and a **cross-realm** callee — the
  unmapped object is built from the *current* realm's `%Object.prototype%` /
  `%ThrowTypeError%` and a lane run pushes no context, so `current_realm` is the
  caller's (compare it against `record.realm`; the funnel pushes the callee's
  realm and is correct). `create_arguments` writes its slot through
  `frame_get_mut`, so this depends on the `nested_frame` bullet below.
- **Every per-activation `Vm` scratch register must be saved AND reset, not
  merely saved.** Use `Vm::save_scratch`/`restore_scratch` (defined in
  `ir.rs`): `ip`, `acc`, `loop_counter`, `loop_num`, the string builder
  (`Builder`, moved out via `mem::replace`, so no memset), `completion`,
  `completion_is_empty`, `switch_disc`, `switch_disc_set`, `chain_short`. The
  lane also saves/restores `lexical_env`, `body_context`, `current_function`,
  `current_new_target`, `strict` (setting the callee's). The self path now uses
  `save_scratch` too — it shared the same latent clobber (a fall-through
  `switch` across a self call). Private fields (`acc`, `loop_counter`,
  `loop_num`) are not reachable from `jit.rs`, which is why the helper lives on
  `Vm` in `ir.rs`.
- **A private frame buffer is NOT `vm.frame`, so `Vm::nested_frame` must point
  at it.** Helpers that read frame slots through `Vm::frame_get`/`frame_get_mut`
  — `builder_bind`/`builder_store` (`s += e` plans, including the register
  body's `LeafOp::BuilderAppend`) and `create_function_decl` (a hoisted block
  function declaration) — would otherwise address the *caller's* frame. Both
  lanes install `nested_frame = Some(buf.as_mut_ptr())` for the run and restore
  the **previous** value (not `None` — a lane can nest, f→g→h, and a hard `None`
  would strip the outer level's frame after an inner return). `frame_get`
  precedence is `leaf_frame_base` (a leaf's frame in `vm.stack`) →
  `nested_frame` → `Frame::get(&self.frame)`. Symptom if forgotten: a callee
  whose `s += n` loop silently wrote the caller's frame slot and returned `""`/
  `0`. `run_jit_body` addresses `vm.frame` directly, so it `debug_assert`s
  `nested_frame.is_none()` — the two must never disagree. The remaining
  `vm.frame` helpers (`catch_bind`, `for_of_*`, `create_arguments`,
  `tail_call_self_vector`) are excluded by `self_call_eligible`.
- **Isolate the shared cursors, on the error path too.** A nested run on the
  caller's `Vm` shares `stack`, `array_index_stack`, `var_ref_stack` and
  `class_stack`; a push it does not pop (a throw mid-`var x = …` or mid-class)
  would otherwise leak an entry into the caller. `save_scratch` records all four
  lengths and `restore_scratch` truncates back to them. This is *not* optional
  once the gate admits builder and function-declaration bodies; the earlier
  `private_frame_safe` refusal was what hid it.
- **Compiled binding resolution must read `vm.lexical_env`, not the running
  `ExecutionContext`.** `load_ident`/`typeof_ident`/`resolve_var_ident`/
  `update_ident` used `context::resolve_binding`, which reads
  `agent.running_context()?.lexical_environment` — the *caller's* env, because
  the lane pushes no context. Use `context::resolve_binding_from(vm.lexical_env,
  …)`. For the funnel path the two are equal, so the change is
  behavior-preserving; without it the whole `resizable-buffer` fixture family
  failed with `ReferenceError: "<global>" is not defined`.
- **`globals_unshadowed` is per-body.** The lane computes it from the callee's
  `environment` (the funnel reads it from the pushed context); the nested ctx
  carries the callee's `global_object`/`global_bits` from `record.realm`.
- **Errors and tail/suspend are handled by gate, not by code.** The gate
  excludes every `TailCall*`/`Yield`/`Await`, so `nested.tail` and
  `DISPATCH_SUSPEND` cannot occur; the lane `debug_assert!`s both and propagates
  a `nested.pending` error through `slow_error` on the *caller's* ctx. Generators
  and async functions are refused explicitly (their call returns an object, not
  a run of the body), as is a class constructor and any non-`EcmaScript` callee.
- **Mode check, not assumed:** the lane is inert in a `--jitless` run and in
  `wasmtest` (no hook), so a wasm sweep is not owed for a lane-only change —
  but `context.rs`/`ir.rs` are linked by `wasmtest`, so run the wasm suites when
  those change.
- **A POSITIVE cached verdict needs a stronger identity than the leaf lane's
  (C1's emit-side lane).** The leaf record's box-address identity
  (`callee_payload`/`callee_hi` + `code_gen`/`epoch`) is safe only because a
  stale leaf verdict is a REJECTION (entry 0 → `call_slow`, which is correct).
  A cached *certified* verdict is POSITIVE (an entry + frame layout), so a swept
  callee's box recycled by a different closure makes the address match while the
  descriptor is another body's — and the lane runs the wrong body. Cache the
  callee's function `id` and compare it (free: the lane already resolves it for
  the `ecma_functions` lookup). `Iterator/zipKeyed/basic-longest` is the
  regression net.
- **The lane runs inside the caller's `try`/for-of/block (C1c).** C1b guarded
  it with `Vm::can_inline_leaf`, so a call while any control stack was non-empty
  fell to the funnel. The isolation C1c actually needs is *only* `env_stack`: a
  lane callee is always a compiled body, the compiled model runs the
  statement-list wrappers (`ListBegin`/`ListEnd`) and the completion saves as
  no-ops, and the gate (`has_shared_vm_step_hazard`) excludes every producer of
  the other stacks — so a nested run cannot touch the caller's `try_stack`/
  pending/for-in/for-of/destructure/yield-star state, and its throw reaches the
  caller's handler through the returned pending error. `has_control_state` is
  just `env_stack.len() > 1`; the save lives in a cold `Vm::save_control_state`
  (state on the traced `Vm::saved_control`), and the at-rest path only resets
  the shared `env_stack`.
- **Keep `save_scratch`/`restore_scratch` small.** Inlining the `SavedControl`
  push/restore blocks bloated them past inlining and taxed *every* call even
  when the branch was not taken (`recursive_fib` 54 → 87 ms, `non-leaf call`
  5.5 → 6.3 ms); they are outlined into `#[cold] #[inline(never)]` helpers. New
  caller-control-state shapes are netted by the two `…_does_not_drift` tests plus
  `installed_jit_certified_callee_inside_caller_control_state_matches_the_interpreter`.
- **The construct lane is the same core (C2a).** `certified_call_inline` is now
  `certified_lane_inline(ctx, callee, this, args, argc, cached, construct)`; the
  construct arm creates the receiver with `construct_this_object` (not
  `OrdinaryCallBindThis`), sets `Vm::current_new_target` to the callee, gates on
  `EcmaFunction::certified_construct_eligible` (base kind, no instance
  fields/private methods, a constructible non-lexical `this`, plus the C1 lane
  eligibility — it admits a NON-leaf body, unlike the leaf-only
  `construct_inline`), and applies the base-return rule (an object/function
  return wins, else the receiver). `step_construct_impl` roots the construct
  args on `vm.stack` for the lane window (a local slice is invisible to the
  collector) and takes the lane only for a JIT caller (`shared` ctx); a derived
  `super()` or instance-field constructor is refused and keeps the general path.
  The receiver is created *after* every refusal (so a declined construct leaves
  no receiver — `construct_this_object` reads an observable `prototype` getter).
  Measured: a non-leaf base constructor 259 → 128 ms / 500k (~2×);
  `construct_churn` is a leaf-constructor row (the no-regress guard).

## 25. The optimizing tier's call lane is the per-step leaf lane, not `call_slow`

A lifted call body must inline its certified leaf callee. `opt_lower`'s
`Op::Call` lowers through `emit_leaf_call` (the G port), reproducing the
per-step `emit_call`: the record-reuse gate, the probe/restamp miss split, the
in-frame/aliased/fill/env lanes, the certified lane, and `call_slow` only as
the universal fallback. Before it, the opt `Op::Call` ran `call_slow`
unconditionally, so admitting a call-heavy body to the tier *regressed* it
(the F10 negative: a leaf call went from inlined to one `call_slow` per
iteration).

- **The lane is not separable.** The leaf record the hit path reads is written
  only by the *probe* (`leaf_call_probe`), which is part of the lane, so a
  "read the record + call the entry" partial always misses. G-a alone (the
  in-frame lane with `call_slow` on every miss) is inert; the probe lane is what
  makes it pay.
- **The opt IR has no push stack, so the argument region is `Abi::work`.** The
  args are materialized at the working-region base (`Op::Call` already does
  this), the leaf frame carves above them, and the call yields an SSA result
  (`emit_leaf_result_tail`: the pending-byte check then the value — no sp
  restore).
- **The non-aliased probe frame sits at `args_ptr + argc*8`.** The per-step
  computed that as its runtime `sp`; the opt lane must compute `args_top`
  explicitly (`fill_leaf_frame` fills there). The aliased frame IS the args
  region, and only that case uses the compiled room check (the non-aliased and
  probe paths let the helpers do the full frame-size check).
- **Verify with a body that lifts AND calls.** The `s = s + leaf(i)` loop shape
  lifts; the `s += leaf(i)` fused-store head bails at `FastLoopHead` (section
  24). A callee carrying a step the lift refuses stops being a certified leaf,
  so it cannot be used to "calibrate" the opt count — `installed_jit_opt_tier_
  leaf_call_inlines_not_call_slow` asserts `opt_compiled_count >= 2` (caller +
  callee) and a small `call_slow` count instead.
