---
name: slag-jit
description: "Load when working on Slag's Cranelift JIT (crates/jit/src, crates/runtime/src/jit.rs, JIT-visible crates/runtime/src/ir.rs) — helper ABI registration, emit_step lowering, block sealing, dispatch sentinels, leaf-call probe, certified iterators (for-of/for-in/AsyncForOf*), suspension (Yield/Await resume dispatch, jit_work), super access, new.target, or destructuring (Destructure* steps, flat binds). Traps: the four-file helper mirror, the pending-error ABI, the dispatch-target chain, the element-via-work-stack-pointer convention, the ForOfBegin boundary span, the compiled for-of close requirements, the sealed-block/back-edge rules, the for_of_advance core contract, resumable-body rules, the destructure rules (flat binds, for-head trap, close gates), the script completion-register writes (compiled steps write vm.completion; eval_program maps the fall-off-end Return(undef)), and the Cut 69 compile threshold (the lookup_info gate, body_has_loop↔step_targets sync, the call_slow→try_jit_leaf chain)."
---

# Slag JIT traps

The JIT (`crates/jit/src`) compiles certified `CompiledBody`s to Cranelift
machine code. `crates/runtime/src/jit.rs` owns the runtime side: the
`JitCallContext`, the `JitSlowPaths` helper table, and the helper
implementations that route compiled code back into the interpreter's
machinery. This skill covers the traps that cost real debugging time;
the interpreter-side VM traps live in `slag-bytecode-vm`.

## 1. The helper-table mirror is four files

Adding a helper touches FOUR places that must stay in lockstep (a missing
one fails to compile, but a mismatched signature surfaces only as a
Cranelift verifier error):

1. `crates/runtime/src/jit.rs` — the `JitSlowPaths` field + `JIT_SLOW_PATHS`
   static entry + the `extern "C" fn` implementation.
2. `crates/jit/src/helpers.rs` — the `Helper` enum variant + `name()` +
   the `JitHelpers` field + `none()` + `get()` + a test double.
3. `crates/jit/src/lib.rs` — `runtime_helpers()` (real table) and
   `helpers_all()` (test doubles).
4. `crates/jit/src/compiler.rs` — the `emit_step` arm that calls it (and
   `max_stack_usage`/`step_name`/`step_targets` entries for the step).

`JitSlowPaths` is `#[repr(C)]` mirroring `JitHelpers` field order, so a
field added to one but not the other is a layout shift — the runtime
installs `&JIT_SLOW_PATHS` directly into the hook, and `runtime_helpers()`
copies it field by field. The `Option<fn>` in `JitHelpers` is how a
missing helper bails a body; the test doubles are real `extern "C"` fns so
the scaffold's `run()` proves the call ABI end to end.

## 2. Helper signatures must match the machine code's sig exactly

The compiler picks a pre-imported signature (`sig_bool` = `(vm, x)`,
`sig_step` = `(vm, step)`, `sig_get_name` = `(vm, x, y)`, `sig_tdz` =
`(vm)`, ...) and passes the helper's *extra* args. A count mismatch is a
Cranelift verifier error ("mismatched argument count") at compile time —
fine — but the *meaning* of each slot is on you. Two conventions that
bite:

- **Step-index helpers** (`create_function`, `enter_block`,
  `enter_per_iteration`, ...) take the step INDEX, not a marshalled
  payload — they read `Step` fields back out of the running body via
  `step_at(ctx, step)`. The step's payload is the authoritative copy (the
  fixup-patched fields, e.g. `ForOfBegin`'s `(top, end)`, are only
  correct read from the body).
- **The working-stack pointer convention**: a helper that must land a
  value where the machine code can reach it (the for-in/for-of fetch
  helpers) takes the current `sp` as a `u64` arg and writes the value at
  `*(stack as *mut u64)`, returning a small code (1 = element, 0 = done).
  The machine code advances `sp` by 8 only on the element path. The
  buffer is rooted for the run's duration (`jit_roots`), so the written
  `Value` stays live. This is the `leaf_call_probe` precedent (it takes
  the `args` buffer pointer).

## 3. The pending-error ABI and the dispatch sentinels

A helper that hits an interpreter error calls `slow_error` (sets
`ctx.pending` + `ctx.error`, returns `Value::Undefined.bits()`). The
machine code's `call_slow` checks the pending byte (ctx offset 0) after
every helper and routes to the error block: `dispatch_error` when the
body has try machinery, a direct return otherwise. NEVER call
`call_slow` from inside an error path (the pending byte is already set —
it re-enters the error block); use `emit_raw_call` there.

The control-dispatch helpers (`return_control`, `break_control`, ...)
return either a step index (the machine code branches over the body's
static `dispatch_targets` via `emit_jump_to_step`) or a sentinel:
`DISPATCH_PROPAGATE` (escaping throw — the pending error is set, return
`undefined`) or `DISPATCH_DONE` (completed return — the value is in
`ctx.dispatch_value`). A helper that can mutate the Vm stacks or realm
count must `bump_leaf_epoch` after the call (via `emit_dispatch_call` or
the `disturbs_leaf_eligibility` classification) or a cached leaf-call
verdict goes stale.

## 4. The sealed-block rules (Cranelift)

- Every transfer target block must exist BEFORE `seal_all_blocks` runs:
  `back_targets` is seeded from `step_targets(step)` for every step (the
  pre-scan in `emit_all`), plus every `dispatch_targets` entry. A block
  that receives a jump from a LATER step and isn't in `back_targets` hits
  the "block is sealed" assertion.
- A `cond_jump`'s fall-through must be `index + 1` (a real step block),
  never a sealed block — the Cut 56 crash.
- The certified do-while for-of/for-in shape: the loop-bottom fetch's
  `back` is a BACK edge (into `back_targets` via `step_targets`); `done`
  is always a FORWARD label (placed after the loop), so its block is
  sealed at its own visit — all its predecessors (the prologue fetch and
  the loop-bottom fetch) were emitted before it.
- A body with a self-tail-call needs a dedicated re-entry block — the
  back edge can never target the function's ENTRY block ("invalid
  reference to entry block").

## 5. Certified for-of/for-in lowering (Cut 57)

`ForInBegin`/`ForOfBegin` open the enumeration/iteration through the
interpreter's shared machinery (`eval::for_in_key_levels` /
`expr::for_of_begin`, which keeps the dense-array fast verdict).
`ForInNext`/`ForOfNext`/`ForOfNextBindLocal` advance via the shared
`Vm::for_of_advance` core — extend THAT core, not the step handler and
the JIT helper separately (they must stay 1:1). The element lands on the
working stack (non-fused) or the frame slot directly (the fused bind,
which is why `ForOfNextBindLocal` needs no stack pointer). `ForOfClose`
pops the boundary and closes a Generic entry; the fast entry has nothing
to close. `ForOfBindLocal` binds inline (the write IS the initialization
— no TDZ check); `ForOfBindGlobal` is exactly the existing `set_global`
helper. A captured lexical head emits `EnterPerIteration` (first env,
pushed) + `PerIteration` (fresh copy per later iteration, replacing
without pushing) — both mark the env context-transparent (the try-cut
TDZ regression trap) and both read their `names` from the step.

**The `ForOfBegin` boundary span is fixup-patched**: the step compiles as
`{ top: 0, end: 0 }` and `Fixup::ForOfBoundary` rewrites it after the
loop compiles. The JIT helper MUST read `top`/`end` from the step payload
— pushing `(0, 0)` breaks `close_for_of_upto`'s span test (`target >
end`), so a `break` to the loop's own end closes the iterator AND the
loop's `ForOfClose` pops the (already-popped) entry AGAIN — the enclosing
loop's entry gets popped instead.

## 6. Compiled for-of bodies must close iterators (the Cut 57 core)

A compiled body that contains for-of machinery (`has_for_of` = any
`ForOfBegin`/`ForOfNext`/`ForOfNextBindLocal` step) has THREE close
requirements the interpreter's `run_inner` Err arm normally satisfies —
the JIT must reproduce them because the callee's Vm stacks are discarded
when the body returns or errors:

- **`Return` routes through `return_control`** (not a direct machine-code
  return) when `has_try || has_for_of`: `control_transfer`'s escape close
  (`close_for_of_return`) runs the iterator's `return` method. A direct
  return silently skips it.
- **A helper's engine error in a non-try for-of body** calls
  `for_of_close_all` (a raw call — the pending byte is already set)
  before returning, mirroring `run_inner`'s uncovered-error close.
- **Both closes skip when `for_of_stepping` is set** — a generic
  `next()` error escapes with the iterator open (spec 14.7.6.2 uses `?`
  on the next call; the flag stays set on the error path of
  `for_of_advance`). `throw_machinery`'s escaping-throw `close_for_of_throw`
  is likewise gated on `!for_of_stepping` (the JIT's `dispatch_error`
  routes `next()` errors through it; the interpreter never reaches that
  arm with the flag set, so the gate is a no-op there).

**Covered next() errors close — matching the interpreter, not the spec.**
A `next()` error whose `ForOfNext` step sits inside a try region (the try
wraps the whole loop) routes through `throw_machinery`'s Catch arm, which
calls `close_for_of_upto` unconditionally — the interpreter closes there
too (a pre-existing deviation; the spec's `[[Done]]` short-circuit is
never modeled). Do not "fix" the JIT to skip the close in that shape: it
must stay 1:1 with the interpreter.

## 7. The register-body interaction

A for-of/for-in body that lowers to `RunRegBody` is fine — the protocol
fetch steps sit OUTSIDE the body slice, so the register executor never
sees them (register ops reject jumps anyway). A `RunRegBody` helper error
inside a for-of body takes the non-try `for_of_close_all` path; the
`error_sp` truncate is only for the try path, and it doesn't matter here
(the body returns immediately — the sp is on the callee's private buffer).

## 8. Visibility and the shared core

The JIT helpers reach Vm state through the ctx's `agent`/`vm` pointers:
`for_in_stack`/`for_of_stack`/`for_of_boundaries`/`for_of_stepping` are
`pub`; `frame_get_mut`, `close_for_of_throw`, `per_iteration_env`,
`context_chain_env`, `store_global_value`, `array_length`/`array_element_get`
are `pub(crate)` (same crate, so the jit.rs helpers can call them).
`EnvRef` is a `Copy` GC handle — never `.clone()` it (clippy
`clone_on_copy`). When you add a step that can leave a for-of entry open
at a `Return`/error, decide its `has_for_of`-relevance explicitly.

## 9. Suspension (Cut 58) — generators and async functions

A certified generator/async-function body compiles: `Step::Yield`/`Step::Await` lower to `yield_suspend`/`await_suspend`, helpers that record the suspension (payload + delegate flag), the machine code's working-stack pointer (`suspend_sp` — the depth `run_jit_body` saves into the NEW traced `Vm::jit_work`), and the continuation step (`vm.ip`), then signal `DISPATCH_SUSPEND` (`u64::MAX - 2`; the machine code `return_`s it directly — the helpers never error, so they are called with `emit_raw_call`, no pending check). Rules that cost debugging time:

- **Resumable bodies are never leaves** — `compile_body`'s `leaf` excludes `is_async || is_generator`, even when the body has no `yield`/`await`. The leaf path runs the body synchronously and returns its value, bypassing `call_async_function`/`call_generator` — an `AsyncFunction('x', 'return x + 1')` call would return `2` instead of a Promise (the regression that broke `async_function_constructor_returns_a_promise`).
- **The compiled entry dispatches on `ctx.resume_kind`**: 0 = normal resume (a compare chain over the body's static `suspension_targets` against `ctx.resume_ip` — `0` means a FRESH run, using the stack parameter's working base — jumping to the continuation block with the resume value pushed at the top of the restored region); 1/2 = throw/return (route `ctx.resume_value` through `throw_control`/`return_control`). A `has_self_tail_call` body shares this forwarding structure — the two are merged entry branches, don't split them again.
- **The working region survives in `Vm::jit_work`** — `run_jit_body` saves `work[..depth]` (`depth = (suspend_sp - work_ptr)/8`) on `DISPATCH_SUSPEND` and returns `Suspended(suspension)`; `run_jit_resume` restores it into a fresh buffer (reserving one extra slot for the resume value) and re-enters the entry. `jit_work` is TRACED and reset with the Vm — it holds live values across the suspension, and the generator/async state Vms live on the agent tables.
- **`yield`/`await` in a certified fast loop takes the SLOT-counter path** — `acc_body_safe`/`acc_expr_safe` return `false` for them, so the loop uses the frame-slot counter, never the machine-local `counter_var`. The counter must survive the suspension; do not "optimize" a suspension body onto the accumulator counter.
- **The drivers install the capture context** — `start_body`/`resume_body` (`generator.rs`) and `run_async`/`resume_async` (`async_await.rs`) set `vm.body_context` = `new_body_context(&env, args)`, `vm.lexical_env` = it, AND the saved ExecutionContext's `lexical_environment` = it, then `setup_certified_frame(scope, &args, this)` fills the frame. The `new_body_context` FALLBACK (a body that captures nothing) must be the closure's `[[Environment]]` (`data.environment`), NOT the instantiated body env — a strict body's `function_declaration_instantiation` installs the Function env as the running context's lexical env, and a nested closure's `LoadContextSlot` at outer-chain depth 0 would then hit it and panic ("a context slot without a capture-context env" — the async-method fixture cluster). `ordinary_call`'s `None => old_env` is the model. A missing context install surfaces as a closure created mid-body failing to see its capture (`"i" is not defined`). Generators bind params at CALL time (errors surface synchronously), so `GeneratorState` carries `args`/`this_value` for the first `next()`.
- **`collect_expr_captures` must walk `Yield`/`Await` arguments** — a `yield function(){ return i }` closure capture was silently unrecorded (the step's argument wasn't scanned); the closure then missed `i`.
- **`yield*` and async generators stay bailed** — certification rejects `Yield { delegate: true }` and the combined async+generator kind; those bodies stay on the env path. The `suspended_at_delegate` arms in `run_jit_resume`/`run_jit_resume_loop` are nominal (dead) now.
- **`run_jit_resume_loop` does not loop** — a single `run_jit_resume` call; a `TailReplaced` outcome hands the rest to `run_jit_body_loop` (the replacement body starts fresh). The drivers take `&mut Rc<CompiledBody>` (the loop may swap it); `async_await.rs` clones the body out, runs the loop on the clone, and writes it back — the borrow checker forbids `&mut state.vm` + `&mut state.body` at once.

## 10. Destructuring (Cut 59) — the flat binds and the close gates

Certification accepts destructuring declaration patterns and assignment targets whose elements are identifiers or nested patterns; the compiler emits the primitive `Destructure*` steps (helpers mirroring the interpreter handlers; the element/key/rest values ride the working stack). Rules that cost debugging time:

- **A certified body NEVER uses the wholesale `Destructure { pattern }`/`DeclInit { pattern }` binds** — they initialize through the ENVIRONMENT machinery, which a certified body never consults (its names are frame/context slots). The compiler routes every certified pattern through `compile_destructure_binding`/`compile_destructure_assign`; the element binds are flat (`InitLocal`/`InitContextSlot` for declarations, `StoreLocal`/`StoreContextSlot`/`StoreGlobal` for assignments — `emit_certified_bind`/`emit_certified_assign_store`). An undeclared assignment target falls back to `AssignIdent` (bails the body at JIT time — correct, slower); a member-target destructure-assign stays env-path (the reference machinery). `SetFunctionName` (anonymous-function defaults) also bails — don't add JIT arms for it in this cut.
- **The for-head pattern trap**: `for (var [...x] = iter; ;)` and lexical pattern heads compile the SAME primitive steps — the var/lexical head paths in `compile_for` previously emitted the wholesale `Destructure`/`EnterLoopEnv` (the two `statements/for/dstr/*` sweep regressions). And a LEXICAL pattern head whose name a body closure captures must REJECT certification (per-iteration freshness covers only ident heads) — but scope the rejection to non-Ident patterns (`fast_path_captured_let_tdz_and_loop_heads` catches an over-broad version that rejected captured Ident heads too).
- **`DestructureUndef`'s fall-through must leave the value on the stack** — use `top()` (read, no pop) + a dedicated pop-block on the DEFAULT path (which consumes the value and jumps to the fixup-patched label via `step_targets`); the fall-through block's sp is untouched, so its multi-predecessor shape (the `jump(after)` from the default path) stays SSA-valid. A pop-then-push in the next block breaks when the next block has two predecessors (the pushed value doesn't dominate it).
- **The close gates mirror `run_inner`'s Err arm**: a step error in a destructure body closes the active not-done iterators (`destructure_close_all` on the non-try error path — a raw call; `dispatch_error` closes BEFORE the handler-table routing, regardless of coverage) UNLESS `destructure_stepping` — a `next()` error leaves the iterator open, including the leftover-stack-entry behavior when a try catches it (stay 1:1 with the interpreter). `DestructureClose` pops BEFORE closing (a throwing `return` must not re-close — bytecode-vm trap 4). The abrupt-resume path (`run_jit_resume`, kinds 1/2) closes via `close_destructures_abrupt` — a `yield`/`await` inside a pattern default can suspend mid-pattern.

## 11. Arguments objects and `typeof` (Cut 60)

`Step::CreateArguments` — the last bail item's `mapped: Some` form — lowers
via a step-index helper reading the `slot`/`mapped` payload. The mapped
arguments machinery was ALREADY complete on the certification/compiler side
(the "mapped arguments slice" moved every simple param into the capture
context, and `compile_body` emits `CreateArguments` once at body entry) —
only the JIT arm was missing. Traps:

- **The mapped helper reads `vm.lexical_env` (the capture context) and the
  running context's `function` (`callee`)** — both are available in a
  certified run (`ordinary_call`/the drivers set `vm.lexical_env` to the
  capture context; the pushed context's `function` is set when
  `scope.arguments_slot.is_some() && !strict`).
- **BOTH `CreateArguments` forms are leaf-excluded — the unmapped form was
  NOT before the fix.** The helper writes the body's `arguments` slot
  through `vm.frame_get_mut`, but a JIT leaf (`run_jit_leaf`) runs on a
  PRIVATE frame buffer (`vm.frame` is the CALLER's) — a helper-written
  frame slot would target the caller's frame, and the unmapped form's
  `vm.call_args` is only filled by `setup_certified_frame` on the non-leaf
  path. The default JIT sweep caught this: strict
  `(function () { return arguments; })()` IIFEs (strict via
  `enclosing_strict` inside a strict script) returned `undefined` — the
  Object/defineProperty, Object/create, arguments-object, Array.prototype.*,
  and Date clusters. The interpreter leaf path was immune (`run_leaf_body`
  sets `leaf_frame_base` so `frame_get_mut` addresses the leaf's own
  region) — the JIT leaf is the only path with the mismatch. Any NEW
  helper that writes a frame slot must be leaf-excluded for the same
  reason; `for_of_next_bind_local` (for-of) and `function_decl_init` are
  already excluded.
- **`TypeofTop` (a `typeof` VALUE operand) is a bonus pair** — `typeof
  arguments.callee` and any member/computed `typeof` need it (the
  `TypeofIdent` unresolvable-reference form stays env-path). It is a pure
  helper (`crux::value::type_of`) — whitelist it in
  `disturbs_leaf_eligibility` and call it with `emit_raw_call` (it never
  sets the pending byte).

## 12. Super property access (Cut 61)

`super.x`/`super[k]` (reads), `super.x = v`/`super.x += v` (writes),
`super.x++`/`super.x--` (updates), `super.x &&= v`/`super.x ??= v` (logical
assign), `super.m()` (calls), and `delete super.x` (the always-ReferenceError,
spec 13.5.1.2 step 4.b — thrown before the key evaluates) certify in non-arrow
method/accessor bodies (`allow_super = allow_this && !is_class_constructor`);
class constructors (`super()` + the this-before-super() TDZ) and arrows
capturing `super` (lexical) never certify. Traps:

- **The base/receiver come from `current_function`** — `certified_this` reads
  the running function's `this_slot`, `certified_super_base` its home
  object's prototype (a STATIC method's home is the class constructor, so
  the base is the superclass constructor). The drivers install it — and the
  async driver sets it for the whole run (Cut 61) — but a leaf never sees
  it, so ALL 12 super steps are leaf-excluded. `GetSuperComputed` was
  MISSING from `steps_are_leaf` (the sweep gap): an inlined leaf would read
  the CALLER's this/home object.
- **`GetSuperComputedKeep` writes the converted key at the passed sp** (the
  element-via-working-stack-pointer convention): the stack shape is
  `[base, key, base, key]` → `[base, key', value]` — the base survives from
  the `GetSuperBase` capture (spec 13.3.7.1: a key whose toString mutates
  the prototype must still see the original base), and the machine code
  advances sp past the converted key and pushes the value.
- **The compiler never emits `UpdateSuperName`/`UpdateSuperComputed`** —
  `super.x++` routes through `ResolveSuperRef*` + `GetVarReference` +
  `UpdateVarReference` (the pre-existing reference machinery; the resolved
  reference records `this_value`), and `super.x &&= v` through `PutVarReference`.
  The two `UpdateSuper*` steps are interpreted but dead in certified bodies
  (like `SuperCall`/`GetVarReferenceThis` — still bailed).
- **`max_stack_usage` accounting**: `GetSuperBase`/`ThisValue` +1; the reads
  net 0 (name) / −1 (computed, incl. the Keep); the assigns pop 2/3 (name)
  or 3/4 (computed, compound) and push 1; the updates −1/−2;
  `ResolveSuperRefComputed` −2 (consumes base+key into the reference
  stack); the name forms and `DeleteSuper` net 0.
- **`super.x` READS THROUGH THE BASE, not the receiver's own properties** —
  `GetValue(super.x)` is `base.[[Get]](name, this)`: the receiver only
  matters when the base's prototype chain finds an ACCESSOR. A data
  property on the instance (`this.x = 10` in a constructor) is invisible
  to `super.x` (returns `undefined` → `NaN` arithmetic). A test/example
  that wants a super READ or UPDATE to observe a value must put it on the
  prototype (an accessor on the base, or the base class's prototype
  property) — the interpreter tests model this (`proto = { x: 42 }`).

## 13. `new.target`, direct-eval CallFast, and heap constants (Cut 62)

The final bail-row miscellany:

- **`new.target` now certifies** in non-arrow bodies (the `FastScopeScan`
  accepts the `new.target` MetaProperty; an arrow's `new.target` is
  lexical — like `this` — and `import.meta` stays env-path). The
  certified path has no FunctionEnv, so the `NewTarget` step reads the new
  per-run `Vm::current_new_target`: the certified construct path sets it,
  a normal call or driver run reads `undefined` (matching the
  async/generator drivers' hardcoded-`undefined` FunctionEnv — async
  functions/generators are not constructible here, so no constructed
  case exists). `NewTarget` stays leaf-excluded DELIBERATELY: a leaf's
  `new.target` differs from the caller's construct context (a regular
  call inside a constructed function must read `undefined`, not the
  caller's constructor) — don't lift the exclusion without giving the
  leaf paths their own per-invocation value.
- **A direct-eval `CallFast` site no longer bails**: `call_slow` gained a
  `direct_eval` flag (a 6th arg — the four-file mirror: `JitSlowPaths`
  field type, `Helper::CallSlow`'s `JitHelpers` field type, the
  `sig_call_slow` import in `emit_call`, and the test double). The
  compiler STILL never emits one (a direct eval always takes the vector
  form — the fast form's eval handling is defense-in-depth), and eval
  bodies never certify anyway, so this is pure completion of the step.
- **Heap-value constants inline their bits**: `const_value` returns the
  NaN-boxed pointer bits for a `String`/`BigInt` (a `Push` or the register
  path's `LoadConst`/`BinConst`) instead of the `push_const`/`load_const`
  helper fallback. Sound because the GC never moves boxes (`Gc` handles
  are `Copy` — the weak-table compaction only clears entries) and the
  value outlives the code: the step's `Push` holds it, the compiled body
  is traced (the unbounded function-site cache, or the active-run tracer
  while a script body runs), and the cache entry that frees the code also
  drops the body.

## 14. Fused global/slot calls and the script completion register (Cut 65)

`CallFastGlobal`, `CallFastSlotStore`, and `CallFastGlobalStore` lower now
(`CallFastSlot` always did); a fused store materializes the arg slots
(TDZ-checked in order) over the working region, calls through the leaf
probe, then TDZ-checks + stores the result. `emit_call` gained an
`emit_fall_through: bool` (9th arg, all call sites updated) because the
store must append AFTER `emit_call`'s merge — emit_call seals and takes
its merge block, so the caller must skip its internal `fall_through` and
emit its own.

**A certified SCRIPT routes through the JIT now** (`eval_program` calls
`run_jit_body_loop` instead of `vm.start`; `run_jit_body_loop` falls back
internally on `Interp`). This made the completion register observable, and
it is the big trap:

- The JIT previously treated the completion steps as no-ops because in a
  certified FUNCTION the body result comes from the machine-code return.
  A script completes by FALLING OFF THE END: `run_inner_inner`'s `None`
  arm reads `vm.completion`/`vm.completion_is_empty`. The JIT never wrote
  them, so every certified script failed with "Illegal control flow at
  the top level" (the machine code's past-the-end `undef` came back as
  `run_jit_body_loop`'s `Completed(Return(undef))`, which
  `completion_to_result` rejects for a script).
- Fix: the compiled `SetCompletion` (pop, write value + `is_empty=false`),
  `ResetCompletion` (write undef + `is_empty=true`), and the
  statement-position `FusedStoreLocal`/`FusedStoreGlobal` (split from
  `StoreLocal`/`StoreGlobal`, which do NOT write — their result is popped
  by a following `SetCompletion`) store straight into the Vm via the
  `runtime::jit::VM_COMPLETION_OFFSET` / `_IS_EMPTY_OFFSET` constants
  (the jit crate cannot name the `pub(crate)` `Vm` for `offset_of!`).
  `eval_program` converts the JIT's fall-off-end `Return(_)` marker to
  the register's `Normal`/`Empty` — scripts cannot contain `return`, so
  every `Return` from that path is the marker.
- `NormalizeCompletion`/`ListBegin`/`ListEnd` STAY no-ops. Justification:
  the register is unobservable mid-run in a certified body (no eval/
  with), and at fall-off-end `Empty` ≡ `Normal(undefined)` at the top
  level; the interpreter's ListEnd-restore always lands on the value the
  JIT's never-reset register already holds (the JIT never does the
  ListBegin reset, and the control statements pair every
  `NormalizeCompletion` with a preceding `ResetCompletion` that
  re-syncs `is_empty`). Don't "fix" them without re-deriving this.
- The compiled completion writes are NULL-GUARDED on `ctx.vm` (the
  scaffold's bare-ctx `run` harness passes a null `vm` and must never be
  dereferenced — the `compile_and_run_fast_counter_loop` crash), and
  GC-safe (`Vm::trace` covers `completion`; the vm is an active-run root).
- The fused call-store steps do NOT write the completion register — the
  interpreter's `CallFastSlotStore`/`CallFastGlobalStore` arms don't
  either. But note the fusion only fires for plain `LoadLocal` args: a
  literal (`s = g(1)`) or the counter (`PushAcc` in the acc-path loop
  body) keeps the `FusedStoreLocal` tail, which DOES write — that
  difference is observable in a script's completion and is tested.

## 15. Validation

`cargo clippy --workspace --all-targets -- -D warnings` clean, then
`cargo test --workspace` green — then REBUILD the sweep
(`cargo build --release -p test262`) and run the full areas (see the
`slag-conformance` skill). The JIT (default) baselines: `language` 23721/0/0/0,
`annexB` 1086/0/0/0, `built-ins` ~23210 pass / 0 fail with 440–450
pre-existing hang wobble on the slow RegExp property-escape /
CharacterClassEscapes / decodeURI / TypedArray / Temporal clusters (load
wobble, not a regression — diff the fail+crash union against a parent
worktree). A JIT e2e test lives in `crates/jit/src/lib.rs`
(`with_jit_agent` runs a script with the real runtime helpers and reports
the compiled-body count); the `installed_jit_*` tests are the behavioral
suite — new steps need one, including the iterator-close shapes
(break/return close, `next()` error does not, body error does).

## 16. Leaf-cache revalidation on a stale epoch (Cut 68)

The per-call-site leaf verdict (`LeafCallSiteCache` in `JitCallContext`) is
only trusted while `cache.epoch == ctx.leaf_epoch`. A "disturbing" helper
(a getter, `valueOf`/`toString`, a nested call) bumps `leaf_epoch`, so the
next visit misses. Before Cut 68 the miss ALWAYS re-probed — a monomorphic
hot call next to a getter probed every iteration (100K probes per loop).
Now the compiled gate re-validates the eligibility state and reuses the
verdict when it is at rest. Traps:

- **The epoch must gate separately from site/callee.** `hit = (site_ok &
  callee_hit) & epoch_ok`; the re-validation runs only when the site AND
  callee still match (`stable`) — a stale epoch on a DIFFERENT site/callee
  must probe, never re-stamp. The re-stamp path (`stale_block` →
  `emit_leaf_state_at_rest()` → ok? store `cache.epoch = live epoch` →
  jump to the HIT block) reuses the cached verdict INCLUDING a cached
  rejection (entry 0 → `call_slow`), which is always correct.
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
- **Two hot call sites thrash a single record — fixed with a set.**
  `leaf_call_cache` is a direct-mapped `[LeafCallSiteCache;
  LEAF_CALL_CACHE_ENTRIES]` (4) set, indexed by
  `(index ^ (index >> 2)) % LEAF_CALL_CACHE_ENTRIES` on BOTH sides: the
  compiler bakes the slot into `emit_call`'s cache-pointer immediate
  (`leaf_cache_entry_offset(index)` — the step index is a compile-time
  constant), and the probe writes `[site % slot]` with the same fold. The
  xor-fold is load-bearing: fused call-store statements land call sites
  4-8 steps apart, and the plain `index % 4` maps a spacing of 4 (or any
  multiple) to ONE slot — the two-hot-sites e2e test's sites at 13 and 21
  (8 apart) both fell in slot 1 and still thrashed at ~200K probes until
  the fold was added. A collision is only a re-probe (the exact `site`
  check still gates reuse) — never a correctness issue, and never a place
  to "fix" by making the check looser. If you change the slot function,
  change it in `leaf_cache_entry_offset` (compiler) AND `leaf_call_probe`
  (runtime) together; the empty record's `site = u32::MAX` never matches
  a real site, so a fresh ctx starts cold in every slot.
- **The probe-count drop is the evidence, not wall time.** The getter's
  own call cost dominates, so A/B wall-time deltas were noise (0.025 vs
  0.024s); the defensible measurements are the probe counts 100K → 1
  (stale-epoch revalidation) and ~200K → 2 (two hot sites) — the
  `installed_jit_stale_epoch_leaf_cache_revalidates_at_rest` and
  `installed_jit_two_hot_leaf_sites_each_cache_separately` e2e tests wrap
  `leaf_call_probe` in a counting fn via
  `JitHelpers.leaf_call_probe` and assert the flat counts across a
  100K-iteration loop.

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
  machine code runs; the caller's site inlines on its NEXT fresh run.
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
(`new` in a certified body), or fail scope and never be seen at all.

**`Step::Construct` is un-lowered.** A certified body containing `new`
reaches the JIT and bails: `Construct` has no `emit_step` arm AND no
`step_name` entry, so the catch-all reports the misleading literal name
`"unsupported step"`. The interpreter's fast construct path is the
construct-inline leaf cache (`run_leaf_construct`, `construct_inline` on
`LeafEntry`); the JIT never grew the arm (`NewTarget` IS lowered — the
Construct CALL is the gap).

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
