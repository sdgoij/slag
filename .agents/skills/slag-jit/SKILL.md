---
name: slag-jit
description: "Load when working on Slag's Cranelift JIT (crates/jit/src, crates/runtime/src/jit.rs, and the JIT-visible parts of crates/runtime/src/ir.rs) — helper ABI registration, emit_step lowering, block sealing, the control-dispatch sentinels, the leaf-call probe, or the certified iterator machinery (for-of/for-in/AsyncForOf*). Documents the non-obvious traps: the helper-table mirror (JitSlowPaths ↔ JitHelpers ↔ test doubles), the pending-error ABI, the dispatch-target compare chain, the element-via-working-stack-pointer convention, the fixup-patched ForOfBegin boundary span, the iterator-close requirements for compiled for-of bodies (Return routing, the engine-error close, the for_of_stepping gate), the sealed-block/back-edge rules, and the for_of_advance shared-core contract."
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

## 9. Validation

`cargo clippy --workspace --all-targets -- -D warnings` clean, then
`cargo test --workspace` green — then REBUILD the sweep
(`cargo build --release -p test262`) and run the full areas (see the
`slag-conformance` skill). The `--jit` baselines: `language` 23721/0/0/0,
`annexB` 1086/0/0/0, `built-ins` ~23210 pass / 0 fail with 440–450
pre-existing hang wobble on the slow RegExp property-escape /
CharacterClassEscapes / decodeURI / TypedArray / Temporal clusters (load
wobble, not a regression — diff the fail+crash union against a parent
worktree). A JIT e2e test lives in `crates/jit/src/lib.rs`
(`with_jit_agent` runs a script with the real runtime helpers and reports
the compiled-body count); the `installed_jit_*` tests are the behavioral
suite — new steps need one, including the iterator-close shapes
(break/return close, `next()` error does not, body error does).
