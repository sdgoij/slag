# The tier's resume-fidelity slice: `Op::Check`

**Status: landed (2026-10-07).** A speculation guard (`Op::Check`) now lowers to a
deopt that resumes the interpreter at a step boundary with the operand stack
exactly as the interpreter would have it, and the entry blocker is resolved by
lifting `Step::LoadIdent` (which makes a body non-leaf, so it runs through
`run_jit_body` rather than the leaf lane). This is the prerequisite
`optimizing-tier-impl.md` §4/§8 names for the guarded, hoisted member read.

## The mechanism

- The entry ABI is `(frame, stack, vm) -> i64` (`compiler.rs:209` `jit_sig`);
  parameter 1 is the working-region base in every entry path (`work_ptr` for
  `run_jit_body`, the region above the frame for the leaf lane). `opt_lower`'s
  `Abi` now carries it as `work`.
- `Op::Check` shape: `args[0]` = the guard condition; `args[1..]` = the live
  operand stack (bottom→top); `imm = Imm::Int(step)` = the resume step. Exit when
  `args[0]` is falsy.
- Lowering (`opt_lower`): `brif(cond_truthy, cont, deopt)`; in `deopt`, store each
  stack value at `work + i*8`, set `ctx.suspend_sp = work + n*8`, `vm.ip = step`,
  `return DISPATCH_DEOPT`; `cont` continues the block, so a guard sits mid-block
  with the CFG intact. This is plan §4's "brif into a per-body deopt block", with
  the condition from `args[0]`.
- `run_jit_body` (`jit.rs`) then truncates `vm.stack` to `work_base + depth` —
  **the fix below** — and `run_compiled_body_on` runs `vm.start` from `vm.ip`.

## The blocker, and how it was resolved

Every body the lift previously accepted was leaf-eligible (`steps_are_leaf`
excludes exactly the call and reference steps the lift also refused), so a lifted
body ran through `run_jit_leaf`, which has no `DISPATCH_DEOPT` handling — the
sentinel surfaced as a runaway recursion. The per-step probe never hit this
because it is gated `deopt_probe_ok = !body.leaf && !has_suspension`
(`compiler.rs:911`).

**Resolution: lift `Step::LoadIdent`** (`BindingLoc::Env` — a global read from a
function body). It is a real read the tier wants, it lowers through the same
`load_ident` helper the per-step path uses on its slow path (a global-value-cell
fast path is a later slice), and it makes the body **non-leaf** — so it runs
through `run_jit_body`, the entry where the guard resume works. `stack_delta` in
the lift needed `LoadIdent => +1` (the stack-depth fixpoint) in the same change.

## The nested-deopt fix (a latent bug the slice exposed)

`run_jit_body`'s `DISPATCH_DEOPT` arm shifted the live operands to the stack
bottom (`copy_within(work_base.., 0); truncate(depth)`). That is a no-op only
when `work_base == 0` (the top-level driver). A **nested certified call**
(`run_certified_call_nested`) carves the callee's frame as a `vm.stack` segment
and sets `leaf_frame_base`, with the working region above it — so the shift
destroyed the frame and `vm.start` then panicked in `frame_get_mut` (empty
frame). The arm now just `truncate(work_base + depth)`: the machine code already
mirrored the operands at `work_base` (right above the frame), and the frame stays
in place for the resumed `vm.start`.

## The diagnostic producer

`opt::lift::OPT_PROBE_DEPTH` (tests only; zero by default) makes the lift emit a
constant-false `Op::Check` at the first non-leaf step whose incoming stack has a
given depth. It exercises the mirror and the resume end to end; a later slice
replaces it with the feedback-driven guarded read.

## Validation

- `cargo test --workspace` 5683 passed / 0 failed (incl.
  `opt_tier_global_ident_read_matches_the_interpreter` and
  `opt_tier_deopt_probe_resumes_the_interpreter`).
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- test262 with the tier on by default: `language` 23,726 / 0, `built-ins`
  23,820 / 0 / 1, `annexB` 1,086 / 0 — all at baseline.

## Next

The mechanism is in place; the producer that consumes it is the guarded, hoisted
member read: a `Check` on the member cell's generation at a loop preheader, so
the unguarded load inside the loop is served while the check holds. That needs
the feedback record to expose the *serving* premise (the `get_member_name`
signature change the I3b note defers to "the guard that reads it").
