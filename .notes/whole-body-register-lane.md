# Scoping: whole-body register programs (the `fib` lever)

Status: **not started**. This note exists so the next session starts from the
measurements instead of re-deriving them.

## Why this and not more micro-slices

`fib` is the only row far from Brimstone: 6.10x `njit/bs`, against 2.36 next
worst (`closures`). Callgrind on `fib(24)` (150,049 activations, 2248 Ir each)
splits an activation into:

| cost | Ir | share |
|---|---|---|
| `run_inner_inner` (step dispatch, 13 steps) | ~630 | 28% |
| same-stack frame: push (~340) + pop (~250) | ~590 | 26% |
| `save_scratch` / `restore_scratch` | ~95 | 4% |
| `fast_call_core` | ~70 | 3% |
| `load_ident_value` (the `fib` callee read) | ~87 | 4% |
| `control_transfer` + `close_for_of_return` (the `return`) | ~75 | 3% |
| everything else (step handlers, `binary_inline`, GC-safe points) | ~700 | 31% |

The last four commits took the cheap parts of this (`save_scratch`'s double
struct copy, `call_args`, `to_boolean`, the numeric compare). What is left is
structural: **28% is dispatch over 13 steps that the register lane could run in
one pass**, and the lane already exists — it just stops at control flow and
non-leaf calls.

The body, for reference (`--print-bytecode`):

```
 0: LoadLocal { slot: 0 }         ; n
 1: BinaryImm { op: LessThan, imm: 2.0 }
 2: JumpIfFalse(5)
 3: LoadLocal { slot: 0 }
 4: Jump(12)
 5: LoadLocal { slot: 0 }
 6: BinaryImm { op: Sub, imm: 1.0 }
 7: CallFastIdent { name: 642, argc: 1, span: .. }
 8: LoadLocal { slot: 0 }
 9: BinaryImm { op: Sub, imm: 2.0 }
10: CallFastIdent { name: 642, argc: 1, span: .. }
11: Binary(Add)
12: Return
```

## The seam already exists

`48c4775` ("give the register lane control flow") added `LeafOp::Jump` and
`LeafOp::JumpIfFalsy` and made `run_leaf_ops` a program-counter loop. **Nothing
emits them yet.** `lower_step` returns `None` for `Step::Jump`/`JumpIfFalse`, and
`lower_leaf_ops` (the whole-body form) is called only for `leaf` bodies — and a
body with a call is never a leaf. So `fib` runs 13 step dispatches per
activation where one register program of ~16 ops would do.

Three things are missing, in dependency order.

### 1. `CallFastIdent` in the register lane

`lower_step` handles `Step::CallFastSlot` and `Step::CallFast` (as
`LeafOp::CallFastSlot`/`CallFast`) but **not** `CallFastIdent`, which is the
shape a script global's callee takes from inside a function body — `fib`'s case.
`LeafOp` needs the equivalent (`name`, `argc`, `span`), executed by the same
`do_call_fast_ident` the step path calls.

This alone is cheap and safe, and it is measurable on its own: with it, a body
whose calls are `CallFastIdent` becomes *lowerable straight-line* — but still
segmented, because step 2/4 are jumps, so expect only a small win from this
slice in isolation (`RunRegBody` per fragment instead of per step).

### 2. Whole-body lowering over the fixup-resolved step array

`lower_leaf_ops` walks `steps` linearly and requires every step to lower. For a
branching body it needs a **step-index → op-index map**: each step emits 0, 1 or
2 ops, so a `Step::Jump(t)`'s target is `map[t]`. The map is buildable in one
forward pass (record `ops.len()` as each step's entry), then `LeafOp::Jump`/
`JumpIfFalsy` targets are patched.

Two traps:

- **A `RunRegBody` inside the run** (a loop body already lowered by
  `lower_leaf_ops_segmented`) must stay its own step; either refuse the body or
  emit a nested-op form. Refusing is the safe first cut.
- **`LeafOp::Jump` is refused by the JIT** (`compiler.rs` returns
  `Unsupported::Step("RunRegBody")` for an unknown op, and `48c4775` says the
  per-step tier deliberately bails a body containing a branch). That is
  acceptable — the body falls back to the *per-step* tier, which still compiles
  it — but it must be checked with `JIT_DUMP_CLIF=1` that JIT mode does not
  regress, per the `slag-jit` skill §7.

### 3. Resumable runs across a call — the actual work

A run with a call in the middle cannot hand the activation to the driver the way
a call *step* can: `run_leaf_ops` has no resume point, so `LeafOp::CallFast`
performs the call in place through `complete_call`, which for an EcmaScript
callee takes `run_certified_call_nested` — a nested `run_inner` with
`save_scratch_full` (it parks ~20 control stacks). That is *more* expensive than
the same-stack lane `fib` uses today (~450 Ir for the pair), so a register run
that calls through `complete_call` would lose.

The usable design is to suspend and resume:

- `LeafOp::CallFast*` executes the fast-call prologue (`do_call_fast*`) and, when
  it withdraws a `PendingCall`, records `(ops, pc + 1)` on the Vm and returns a
  "suspended" marker instead of completing.
- `Step::RunRegBody`'s handler turns that into `VmOutcome::Call`, exactly as the
  step path does, so the driver's existing `try_push_inline_call` /
  `pop_inline_call` run unchanged.
- On the pop, the driver sees a resume record and re-enters the same
  `Step::RunRegBody` at the recorded pc instead of at `ip + 1`.

The lifetime argument that makes a raw `(ops: *const [LeafOp], pc)` sound:
`InlineCallFrame::caller` already holds the caller's `Rc<CompiledBody>` on every
non-tail call (that is what `restore_caller` uses), and for a `Borrowed` body the
driver's `root` outlives the loop. So the ops array cannot be freed while the
frame is suspended. This needs a comment and ideally a test that frees-and-
compiles competing bodies while a run is suspended.

Error paths must clear the record: a throw inside the callee pops the frame and
resumes the caller at the call step's continuation (the existing
`pending_error`/`route_step_error` machinery), so a stale resume record must be
dropped on every `pop_inline_call` that did not come from a register suspension.

### Expected payoff, honestly

- Dispatch 630 → ~16 ops × ~25 Ir ≈ 250-400, saving ~250-380 Ir (11-17%).
- Calls stay on the same-stack lane at ~450/activation, unchanged.
- So `fib` 2248 → ~1900 Ir, **−15%**, `njit/bs` 6.10 → ~5.2.

That is the whole prize. It is not a 2x. Getting to parity with a bytecode
interpreter on this row needs the same-stack frame (590 Ir) and the
`ExecutionContext` push broken too, and those are not available without giving up
per-activation stack traces (see the `a_same_stack_recursion_reports_every_level`
test).

## Cheaper rows still on the table

If a session does not want the above, these are measured and independent:

- `local` 1.79, `fn_object` 1.97, `closures` 2.36: the leaf-call prologue is
  ~850 Ir/call on `local` against a ~150 Ir body. Profile targets, in order:
  `do_call_fast_slot`'s Vec work (`push`/`truncate`: 71 Ir), slice indexing in
  `frame_get` (80 Ir), the `args_buf = [Value::Undefined; FAST_CALL_MAX_ARGS]`
  zero-fill (paid on the Alias path, which never reads it), and
  `leaf_completion_result` (13 Ir).
- `fn_object` 1.97: `member_cell_get_map` 13% of the row plus the computed-member
  paths.
- `object` 1.40 / `fn_object`: the top-level script sync is now two steps, so
  what is left is the member-read/write helpers.

## Measurement discipline for whoever takes this

- Every claim next to an **inert control** (a semantically empty perturbation of
  the parent), in the mode being claimed. An inert `GLOBAL_CELLS` 256 -> 512
  build reproduced a 14% `method` swing this session.
- `callgrind` instruction counts, not wall clock, for anything touching `local`
  (its Ir is stable to 0.006% while its time moves ±9% with binary layout).
- Sweeps before committing (`language`/`annexB`/`built-ins` at
  `--timeout 15`), plus the corpus in both modes for result parity.
