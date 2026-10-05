---
name: slag-jit
description: "Load when working on Slag's Cranelift JIT (crates/jit/src, crates/runtime/src/jit.rs, JIT-visible crates/runtime/src/ir.rs) — helper ABI registration, emit_step lowering, block sealing, dispatch sentinels, leaf-call probe, certified iterators (for-of/for-in/AsyncForOf*), suspension (Yield/Await resume dispatch), super access, new.target, or destructuring (Destructure* steps, flat binds). Traps: the four-file helper mirror, the pending-error ABI, the dispatch chain, the element-via-work-stack convention, the certified-callee lane, the ForOfBegin boundary span, the compiled for-of close requirements, the sealed-block/back-edge rules, the for_of_advance core contract, resumable-body rules, the destructure rules (flat binds, for-head trap, close gates), the script completion-register writes (compiled steps write vm.completion; eval_program maps the fall-off-end Return(undef)), and the Cut 69 compile threshold (the lookup_info gate, body_has_loop↔step_targets sync, the call_slow→try_jit_leaf chain)."
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

**The fused `ForOfNextBindLocal` inlines a dense-array cursor (G17).**
`Step::ForOfBegin` carries `cursor: (array_slot, index_slot)` — two hidden
frame slots from the bytecode compiler's `alloc_hoist_slot` (`NO_FOR_OF_CURSOR`
when the head has no fused bind, e.g. a captured/context binding or the
non-certified path). The `for_of_begin` helper seeds them: the array slot gets
the `ForOfState::FastArray` Value, or `undefined` for every other verdict, so
the compiled advance can test it as an object tag. The compiled
`ForOfNextBindLocal` then reads `elem_ptr[index]` straight from the live
`array_dense` cursor with no helper call, mirroring
`emit_dense_element_read_into`'s invariants (a live cursor, `index < elem_len`,
a non-hole element — the `elem_len <= length` dense invariant is what makes
`index < elem_len` imply `index < length`, exactly as the compiled `a[i]`
relies on). The traps:

- **The Vm entry, not the frame slot, is authoritative.** The inline path
advances its own slot without touching `vm.for_of_stack`, so any step that
cannot be served inline must first sync the entry. The first decline (a
hole, the end, a receiver that stopped being dense) sets the ARRAY slot to
`undefined` — so every later step takes the helper — and calls
`for_of_fast_next(slot, index)`, which writes `index` into the innermost
`Fast` entry before running the shared `for_of_advance`. Skipping the
abandon makes the next step re-read the (now stale) slot index and yield the
same element twice; skipping the sync makes the helper re-yield what the
inline path already consumed.
- **A frame-slot cursor, not a per-context one, is required.** A
`JitCallContext` lives only for one synchronous compiled call, so a cursor
there would be lost across a generator suspension and shared by nested
invocations. Frame slots survive all three (nesting, recursion, suspension);
a `yield`/`await` inside a for-of body resumes with its cursor intact.
- **The index slot holds a raw `u64`, not a `Value`.** Only the compiled read
touches it; its tag bits stay 0, so a frame scan cannot mistake it for a
heap pointer. The array slot holds a real `Value` and is additionally rooted
by the Vm's `for_of_stack` entry for the loop's lifetime.
- **`Fixup::ForOfBoundary` must pattern-match `ForOfBegin`'s `cursor`** when it
rewrites `(top, end)` — a wholesale reconstruction resets it to `(0, 0)`
(the same trap the bytecode-vm skill records for `ForOfNext`'s `back`).

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

A certified generator/async-function body compiles: `Step::Yield`/`Step::Await` lower to `yield_suspend`/`await_suspend`, helpers that record the suspension (payload + delegate flag), the machine code's working-stack pointer (`suspend_sp` — the live top of the region `run_jit_body` keeps on `vm.stack`), and the continuation step (`vm.ip`), then signal `DISPATCH_SUSPEND` (`u64::MAX - 2`; the machine code `return_`s it directly — the helpers never error, so they are called with `emit_raw_call`, no pending check). Rules that cost debugging time:

- **Resumable bodies are never leaves** — `compile_body`'s `leaf` excludes `is_async || is_generator`, even when the body has no `yield`/`await`. The leaf path runs the body synchronously and returns its value, bypassing `call_async_function`/`call_generator` — an `AsyncFunction('x', 'return x + 1')` call would return `2` instead of a Promise (the regression that broke `async_function_constructor_returns_a_promise`).
- **The compiled entry dispatches on `ctx.resume_kind`**: 0 = normal resume (a compare chain over the body's static `suspension_targets` against `ctx.resume_ip` — `0` means a FRESH run, using the stack parameter's working base — jumping to the continuation block with the resume value pushed at the top of the restored region); 1/2 = throw/return (route `ctx.resume_value` through `throw_control`/`return_control`). A `has_self_tail_call` body shares this forwarding structure — the two are merged entry branches, don't split them again.
- **The working region survives on `vm.stack` (C0d; there is no `Vm::jit_work` any more)** — on `DISPATCH_SUSPEND` `run_jit_body` normalizes the live region to the stack bottom (`vm.stack.copy_within(work_base..work_base+depth, 0)` with `depth = (suspend_sp - work_ptr)/8`, then `truncate(depth)` — the same shape the DEOPT path uses) and returns `Suspended(suspension)`; `run_jit_resume` re-derives the base as the bottom and the live depth as `vm.stack.len()`, extends the region for the resume value (`+ 1` when the resume pushes), and re-enters the entry. The region is traced with the Vm (the generator/async state Vms live on the agent tables and are traced), and the driver does not touch the suspended Vm's stack between suspend and resume, so a resumable body's base is 0 by construction. Because the operand stack stays on `vm.stack`, an `Interp` fallback resume (no compiled code) continues with the stack intact — the old side-buffer copy discarded it.
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
  `TypeofIdent` unresolvable-reference form lowers through `typeof_ident`,
  which answers `"undefined"` for it — Cut 93). It is a pure
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
It cost a plan entry: `.notes/non-leaf-jit.md` §6 named `Construct` as the next
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

## 19. The read-side value cells and the store discipline

`member_value_cells` is the read-side data-property value cache (16 slots,
`#[repr(C)]`, indexed `(object_id ^ atom) & (MEMBER_CELLS - 1)`). Any probe that
validates it (`emit_member_cell_probe`, and G8's computed read) must agree with
how the STORE paths keep it current, and the store discipline has two regimes:

- A STRUCTURAL change (define, delete, accessor conversion, map transition)
  bumps `JsObject::generation`, which invalidates every generation-validated
  cell.
- An IN-PLACE VALUE write does NOT bump the generation. `set_key`'s fast path
  bumps, but `JsObject::write_data_property_slot` (the interpreter's L1a warm
  store) and `JsObject::write_data_property` (the JIT's compiled store, via the
  `SetMemberSlot` helper) deliberately leave the generation put and REFRESH
  `member_value_cells[member_cell_index(id, atom)]` with the new value instead.

So a cell keyed by anything other than the atom cannot be kept current by the
store paths — they know the atom, not the key box. That is the trap G8 hit: a
computed-read cell keyed by the key Value's identity and holding the VALUE
validated the receiver generation alone, so `o["a"] = 2` after a cached
`o["a"]` served the stale value (the corpus caught it: 59 `harness` fixtures —
`verifyProperty`'s read/write/restore of a computed key). The landed shape is a
**box → atom map** (`computed_read_cells`, keyed by the key's own bits): both
sides immutable (strings are immutable, the interner is append-only), so it
never needs invalidation, and the compiled probe takes the VALUE from
`member_value_cells` — exactly the interpreter's `member_cell_get`. The probe is
three branchy blocks in `emit_element_read` (Object tag → `is_string(key)` →
box compare → `(live_id ^ atom) & (MEMBER_CELLS-1)` validated on
id/name/generation), so a String-keyed `o[k]` on a plain object is served with
no `intern` and no helper (`str_key.js` 52.7 → 10.6 ms, `control/for_in.js`
34.9 → 10.54 ms).

Corollaries for any new cell:

- If it caches a VALUE, either key it by the atom (so the store paths can
  refresh it) or make it generation-validated AND ensure no in-place write can
  change what it holds. A value cache keyed by a key box is not maintainable.
- Never assume a write bumps the generation. Check the store path first
  (`warm_store_*`, `write_data_property*`, `SetMemberSlot` — all refresh,
  none bump for an in-place value write).
- `the_emitter_and_runtime_computed_read_slots_agree` pins the slot arithmetic
  and `installed_jit_computed_read_after_inplace_store_matches_the_interpreter`
  pins the store-visibility contract; mutation-check both when the probe
  changes.

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
