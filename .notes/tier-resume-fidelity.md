# The tier's resume-fidelity slice: `Op::Check`

**Status: blocked on a leaf-lane dependency (2026-10-07).** The mechanism was
implemented and its lowering verified sound, but it cannot be exercised
end-to-end yet — every body the lift currently accepts is **leaf-eligible**, and
the leaf lane has no `DISPATCH_DEOPT` handling. The code was reverted to keep the
tree free of an untested mechanism; this note records the design and the exact
blocker so the next slice starts from the finding, not from scratch.

## Goal

Make the optimizing IR able to lower a speculation guard (`Op::Check`) that, on
failure, resumes the interpreter at a step boundary with the operand stack
exactly as the interpreter would have it. This is the prerequisite
`optimizing-tier-impl.md` §4/§8 names for the guarded, hoisted member read. The
`read` row is still only parity; the lever is a *hoisted* read whose premise (the
member cell's generation) is checked once at the loop preheader, so the
unguarded load inside the loop is safe only while the check holds — a
per-activation exit, not just body retirement. `Op::Check` is that exit.

## What already exists (verified)

- The entry ABI is `(frame, stack, vm) -> i64` (`compiler.rs:209` `jit_sig`).
  Parameter 1 is the working-region base in **both** entry paths — `work_ptr` for
  `run_jit_body` (`jit.rs:6720`) and the leaf's working region above its frame for
  `run_jit_leaf` — so the base a guard needs is available everywhere; the
  lowerer's `Abi` simply drops it today (`opt_lower.rs:55`).
- The per-step deopt probe (`compiler.rs:5444` `emit_deopt_probe`) is the model:
  store the working `sp` into `JitCallContext::suspend_sp`, set `vm.ip = step`
  (`VM_IP_OFFSET`), return `DISPATCH_DEOPT` (`u64::MAX - 3`, `jit.rs:4643`).
- `run_jit_body` (`jit.rs:6823`) implements the resume: on `DISPATCH_DEOPT` it
  derives `depth = (ctx.suspend_sp - work_base)/8`, `copy_within(work_base..,0)`,
  `truncate(depth)`, bumps `JIT_DEOPT_COUNT` (`pub`, `jit.rs:637`), returns
  `Interp`; `run_compiled_body_on` (`function.rs:2827`) then breaks and runs
  `vm.start` from `vm.ip`.
- `verify` accepts `Term::Unreachable` (`verify.rs:138`); the lift threads the
  live operand stack as `stack: &mut Vec<ValueId>` through `emit_step`
  (`lift/mod.rs:395`).
- `Op::Check` is a `pure` op with no producer; `cse`/`dce`/`licm` already refuse
  to remove or hoist it; `opt_lower` refuses it (`opt_lower.rs:314`).

## The mechanism (implemented, reverted)

1. `Abi` gains `work = entry_params[1]`.
2. `Op::Check` shape: `args[0]` = the guard condition; `args[1..]` = the live
   operand stack (bottom→top); `imm = Imm::Int(step)` = the resume step. Exit
   when `args[0]` is falsy.
3. Lowering: `brif(cond_truthy, cont, deopt)`; in `deopt`, store each stack value
   at `work + i*8`, set `ctx.suspend_sp = work + n*8`, `vm.ip = step`, `return
   DISPATCH_DEOPT`; `cont` continues the block (so a guard sits mid-block with the
   CFG intact — plan §4's "brif into a per-body deopt block", with the condition
   from `args[0]`). This was implemented and lowered cleanly; a non-firing guard
   (constant-true condition) left the tier bodies at interpreter parity, proving
   the lowering itself is sound.

## The blocker (the finding)

**Every body the lift currently accepts is leaf-eligible, so it never reaches
`run_jit_body`.** `leaf = scope.is_some() && !async && !generator &&
steps_are_leaf` (`ir.rs:25671`), and `steps_are_leaf` (`ir.rs:24300`) excludes
calls and the reference/ident machinery — exactly the steps the **lift also
refuses**. So a lifted body has no calls and is a leaf (loops do not disqualify
it). The interpreter's call path runs such a body through `run_jit_leaf`
(`ir.rs:13555`), which reads the machine-code result as a value: a
`DISPATCH_DEOPT` sentinel is taken as the call's return, observed as
`RangeError: Maximum call stack size exceeded`.

The per-step probe never hits this because it is gated `deopt_probe_ok =
!body.leaf && !has_suspension` (`compiler.rs:911`).

**"Just decline and re-run in the interpreter" is not sound for a leaf.** The
leaf prefix can have observable side effects — a leaf may read `o.x` through a
getter (member reads are leaf-legal), so re-running the body from step 0 runs the
getter twice. The leaf lane also shares `vm` with its caller, so the guard's
`vm.ip` write would clobber the caller's `ip` (the leaf's frame + working region
is one `vm.stack` segment starting at `frame_base`, and no `run_jit_body` owns
the ip here).

## Unblock paths

1. **Lift call-containing bodies.** A body with a call is not a leaf, so it runs
   through `run_jit_body`, where the deopt resume already works. This is the
   plan's real gate for the read lever anyway (a hoisted guard wants a loop
   preheader, and the target rows are call-bearing), so it is the recommended
   path: extend the lift to `Step::CallFast`/`Call` (through the same helpers the
   per-step path uses) and the guarded read lands on top.
2. **Give the leaf lane a real resume.** Not a re-run: the lane would have to
   save/restore the caller's `vm.ip` around the call, mirror into the leaf's
   working region (`region_ptr + frame_size*8`), and rebuild the operand stack —
   while ensuring the prefix's side effects are not repeated. More intricate and
   lower-value than (1).

Until one lands, `Op::Check` emission must be gated on `!body.leaf` (mirroring
`deopt_probe_ok`), or no producer may emit a `Check` into a leaf-eligible body.

## Validation (of the reverted mechanism)

- The lowering lowered and matched the interpreter with a non-firing guard; the
  tier's `OPT_COMPILED` advanced.
- With a firing guard the leaf lane corrupted the result — the blocker above.
- Nothing was committed; the working tree is clean.

## Open decisions

1. `args[0]` as the condition vs a new `Inst` field (chosen: `args[0]`).
2. Unblock via (1) call lifting (recommended) or (2) a leaf-lane resume.
3. The real guard's condition: the member cell's generation compare, emitted by a
   lift-side transform once the feedback record exposes the *serving* premise —
   the `get_member_name` signature change the I3b note defers to "the guard that
   reads it".
