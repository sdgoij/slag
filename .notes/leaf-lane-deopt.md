# Extending the deopt resume to the leaf lane

**Status: L2 + L1 landed (2026-10-07).** Follows the typer arc
(`.notes/tier-typer.md`). The leaf lane now resumes a deopt; the lift emits the
typer's read guard for leaf bodies too.

## Landed (2026-10-07)

**L2 — a deopt-bearing body is never inlined by a compiled caller.**
`JitCompiledInfo` gains `deopts` (set when the lowered IR contains a guard), and
the machine-code inline lanes refuse it: `leaf_call_probe` (clears the record and
returns 0), `certified_verdict` (`CertifiedInlineInfo::empty()`),
`try_shared_construct_leaf` (`Ok(None)`), and `run_jit_body`'s self-call lane
(`self_inline_ok: self_call_ok && !info.deopts`). This was a **latent soundness
hole in T2 as it stood**: a guard-bearing non-leaf body could be inlined by a
compiled caller, and a deopt would then surface as a bogus value. The typer is
off by default, so the hole was only reachable with `SLAG_TYPER=1`.

**L1 — `run_jit_leaf` resumes a deopt.** On `DISPATCH_DEOPT` the lane no longer
pushes the sentinel as a value: it truncates `vm.stack` to `frame_base +
offset` (the frame + the mirrored operands — the layout matches the interpreter's
leaf layout), sets the interpreter leaf-run state the compiled path never touched
(`completion`, `loop_counter`, `strict`, `chain_short`, `call_args`), and runs
`run_inner_inner` from `vm.ip`. Resuming mid-body (not re-running the whole body)
is what keeps a read's side effect from firing twice — pinned by a
getter-counting test. `try_shared_construct_leaf` is refused (L2) rather than
taught the resume.

The lift's `!body.leaf` gate is now lifted: a leaf body takes the read guard.

Measured (`run_typer.js`, guard on vs off on the tier, min-of-7 interleaved):
`read_loop` (leaf) **0.956x** (was 0.992x — the guard had no effect on it before
L1), `read_loop_nonleaf` **0.929x**, `licm_loop` 0.968x; the arithmetic/loop rows
without a read are ~1.000x. Gates: `cargo test --workspace` green (jit 316/0);
clippy `--workspace --all-targets -- -D warnings` clean; test262 `language`
23,726/0/0/0 and `built-ins` 23,820/0/1/0 with `SLAG_TYPER=1` (leaf bodies now
exercise the deopt resume across the suite).

The typer stays **off by default** (`.notes/tier-typer.md` T3): the win is real
but small, and the residual risk is a `read`-that-feeds-no-arithmetic site paying
a guard check for no payoff (`read_bare` ~1.03x, within noise).

---

**Below: the original scope.**

## Why the cap exists

`steps_are_leaf` admits the member/element read steps, so a body whose only
"impure" work is `o.x` is leaf-eligible — and the leaf lanes treat the compiled
entry's return as a *value*. A speculation guard lowers to `return
DISPATCH_DEOPT` (`u64::MAX - 3`), so a deopt in a leaf body would surface as a
bogus `Value` instead of resuming. The lift therefore suppresses the guard for a
leaf body (`guard_reads && !body.leaf`, matching the `OPT_PROBE_DEPTH`
diagnostic). The `read` row's `read_loop.js` is leaf, so the guard cannot help it.

## The model: the non-leaf lane already resumes

`run_jit_body` (`runtime/src/jit.rs`) is the template. A guard's machine code has
already set `vm.ip` to the resume step and `ctx.suspend_sp` to the live
working-region top; the runtime:

1. truncates `vm.stack` to `work_base + depth` (leaving a nested certified call's
   carved frame in place — the shift-to-bottom suspend shape would destroy it),
2. returns `JitRunOutcome::Interp`.

`run_compiled_body_on` then breaks its driver loop and calls `vm.start(agent,
&ir)`. The frame was set up **once, before the loop** (`setup_certified_frame`),
and `run_inner_inner` dispatches from `self.ip` — so the interpreter re-executes
from the guard's step with the mirrored operand stack and the live (mutated)
frame. That hoisted frame setup is the load-bearing detail.

## The leaf lanes that invoke the compiled entry

| lane | site | deopt today |
|---|---|---|
| runtime-driven leaf | `Vm::run_jit_leaf` | no — pushes the return as a value |
| shared-ctx construct leaf | `Vm::try_shared_construct_leaf` | no |
| compiled caller inlines a leaf callee | `leaf_call_probe` / `leaf_call_env` (`emit_call`'s leaf-inline) | no — the *caller's* machine code gets the return |
| certified-callee lane | `certified_lane_inline` / `self_call_inline` | no — same, machine-code inline |

The last two are the hard half: the "caller" is compiled machine code, so a
`DISPATCH_DEOPT` return has nowhere to go. The first two the runtime drives, so
the runtime can take over.

## Slice L1 — the runtime-driven lane (tractable)

For `run_jit_leaf` and `try_shared_construct_leaf`, on `DISPATCH_DEOPT`:

1. truncate `vm.stack` to `work_base + depth` (as `run_jit_body` does),
2. run the leaf body interpreted **from `vm.ip`** with its already-materialized
   frame (the leaf lanes fill the frame themselves — this/params/TDZ/undefined,
   the same content `setup_certified_frame` produces),
3. unwrap the completion the leaf lane's way (`leaf_completion_result`) and push
   it, exactly as the normal tail does.

What that needs:

- **A "resume a leaf interpreted from `vm.ip`" entry.** `run_leaf_body` sets
  `self.ip = 0` and re-dispatches from the start, so it must be split into a
  setup half and a dispatch half (or take a "resume at `self.ip`" flag). The
  dispatch is `run_inner_inner`, which already starts at `self.ip`.
- **Vm-field state preserved for the run**: `leaf_frame_base` (so a helper's
  `frame_get` reads the leaf's own frame), `body_context` / `lexical_env` (the
  env swap `run_jit_leaf` does), `array_index_stack` (the leaf's literal
  balance), `current_function`, `globals_unshadowed`.
- **Rooting**: the interpreted run must be covered by `ACTIVE_RUNS` /
  `with_leaf_run` like the compiled run was.
- **Errors**: a throw propagates raw to the caller (the leaf contract), not
  through a nested error path.

L1 alone helps only leaf bodies the runtime drives. A leaf body a *compiled
caller* inlined still cannot deopt, so L1 must ship with an inline-refusal rule:

## Slice L2 (required for L1 to be sound) — refuse the machine-code inline

A body containing a guard must not be inlined by a compiled caller (`emit_call`'s
leaf-inline, `certified_lane_inline`). Two options:

- **(i) Refuse to inline a guard-bearing callee** (keep it leaf, route the call
  through `call_slow`/`run_jit_leaf`, which handles the deopt). Cost: lose the
  inline for such callees; it is a new eligibility rule in the caller's emitter.
- **(ii) Teach the caller's machine code a deopt path for the inlined call**
  (mirror the caller's stack at the call step, set `vm.ip`, return
  `DISPATCH_DEOPT`). Correct but large — the caller's stack, its own guards and
  its resume all become the guard's responsibility.

Recommend (i).

## Cost / benefit

- **Benefit**: the typer's read guard applies uniformly, including the leaf
  `read` row. Extrapolating the measured non-leaf win (~5.6%), the `read` row
  might gain a few percent; the arithmetic itself is already tag-free (the
  `narrow` fix) and the read already inlined (G2b), so the residual is the one
  read-side tag check per arithmetic op.
- **Cost**: L1 is a moderate, delicate runtime change (split `run_leaf_body`, a
  resume path, state/rooting); L2 is a new inline-eligibility rule.
- **Risk**: the leaf lane is the most delicate surface in the engine — a shared
  `JitCallContext`, a frame carved from the *caller's* `vm.stack` segment, and
  rooting that must cover the interpreted run. A wrong resume is silent
  corruption, not a crash.

## Recommendation

**Defer** unless a bigger lever needs typed leaf bodies. The read's real gain
already landed (the inline read + the tag-free arithmetic from the `narrow`
fix); the guard's remaining contribution is small and mostly non-leaf. If
pursued, land L2's refusal first (behavior-neutral: no leaf body has a guard
yet), then L1, then flip the lift's `!body.leaf` gate — each gated by a full
conformance sweep, since a deopt resume bug is a silent wrong-program risk.
