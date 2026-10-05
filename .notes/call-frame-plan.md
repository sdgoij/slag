# The call/frame model: one frame, not one Vm per call

Plan of record for architectural opportunity 1+4 in the perf survey: the
non-leaf call and the construct both pay a per-call `Vm` allocation and a
`JitCallContext` rebuild because the engine has **two disjoint frame
models**. This document is the plan to collapse them into one frame that
the interpreter and the JIT both address — the thing V8 and SpiderMonkey
have had since their bytecode VMs.

No code lands from this document until a stage is written up and probed;
each stage owes the full gate (`.notes/perf.md`, "Working rules").

## 0. The measurements this exists to move

From `.notes/perf.md` (release, isolated probes):

| shape | cost | where |
|---|---|---|
| inlined certified leaf call (jit) | ~6–7 ns | `:8814` |
| certified **non-leaf** call (jit) | ~126 ns recursive / ~158 ns pooled | `:8814`, `:10750` |
| `new C()` empty body (jit) | ~128 ns, vs ~22 ns for a plain call of the same body | `:8453` |
| `{}` / `[]` / closure create | ~70–80 ns / ~110–230 ns / ~1.4 µs (node ~5 / ~2.7 / ~31 ns) | `:6953` |

`recursive_fib` is 3.44 M non-tail calls; every callee that itself calls or
recurses (the common real shape) pays the non-leaf path. The boxed-pool fix
landed (−38%), but it only made the copy cheaper — the model is unchanged:
`ordinary_call → run_compiled_body → take_vm + execution-context push +
per-run JitCallContext rebuild`, plus, for a construct, a one-FFI
`step_construct` that re-derives eligibility and rebuilds the ctx again.

## 1. What V8 and SpiderMonkey actually do

Studied this session from the vendored checkouts/docs (`v8/docs/runtime/*`,
`v8/docs/interpreter/*`, `v8/docs/compiler/sparkplug/*`, and
`js/src/jit/JitFrames.h`).

**V8.** One fixed-size stack frame per function, layout fixed in
`src/execution/frame-constants.h`: parameters at negative offsets, a fixed
header (return address, saved FP, context, JSFunction, **argc**), an
unoptimized header (BytecodeArray, bytecode offset / FeedbackCell,
FeedbackVector), then the register file at positive offsets. Ignition
executes it; **Sparkplug emits machine code with the identical frame** ("the
stack frame layout for a Sparkplug-compiled function is identical to that of
an interpreted function; whenever the interpreter would have stored a
register value, Sparkplug stores one too"), so OSR and stack walking are
free. TurboFan/Maglev use a *different* optimized frame, and deopt rebuilds
the interpreter frame from a translation opcode stream
(`docs/runtime/deoptimization.md`) — the machinery we get almost for free
because our compiled stack already mirrors `Vm::stack`. Tiering is an
interrupt budget on the `FeedbackCell` decremented at back-edges and
returns; code swaps go through a `JSDispatchTable` handle. V8 **deleted the
arguments adaptor frame in 2020** ("we now have a simpler translated inlined
frame arguments", `59b4dd7a5fa`) — argc lives in the frame, not a trampoline.

**SpiderMonkey.** `JitFrameLayout` (shared by Baseline and Ion) is
`{ callerFramePtr, returnAddress, FrameDescriptor }`, then a `calleeToken`,
then **`this` and the actual args by offset** (`offsetOfThis`,
`offsetOfActualArg`). `FrameDescriptor` packs the frame type **and
`numActualArgs`** — the callee reads the caller's argc from the frame, so
there is no adaptor (the `RectifierFrameLayout` handles the mismatched-`this`
cases inline). ICs run as `BaselineStubFrameLayout` **on the same stack**,
with a stub pointer and up to 255 locally traced values for the GC, and
`InlinedICScript` for trial-inlined scripts. Bailout is
`InvalidationBailoutStack` (saved regs + fp + return address) +
`ResumeFromException` (framePointer, stackPointer, target, kind, exception).
JIT→C++ is an `ExitFrameLayout` with a typed footer (VMFunctionId) — the
native boundary is a **frame**, not a struct reboot per call.

**SpiderMonkey's wasm adds a sharper target** (`.notes/spidermonkey-wasm.md`). Its `Instance` (= `TlsData`) is one **register-held pointer per module**, with the hot fields first in an asserted compact-offset region, **globals inlined into it**, and the GC state (nursery position, store buffer, incremental-barrier address, alloc sites) reachable from it — so a call switches a pointer instead of rebuilding a context, and compiled code allocates and runs barriers inline. Compiled frames are traced through per-code-range `StackMaps` (`Instance::traceFrame`), not a conservative scan. Tiering is per *function*: a `jumpTable_` for calls, `requestTierUpStub`, and patchable far-jump call sites (`CallFarJump`), with one shared stub block across tiers (`prepareTier1`). That is the shape §3.4 should aim at.

**The convergent lesson.** The frame is the tier interface. Args, `this`,
the callee, the return address, the saved parent, and the per-activation
bookkeeping all live in one described region on one stack; the interpreter,
the baseline JIT, and (via translations) the optimizing JIT all address the
same region. A call is a frame push.

## 2. The diagnosis in this engine

We have the two models the lesson says must be one:

- **Interpreter (`Vm`)**: a per-activation value stack (`Vm::stack`), a fixed
  inline/heap frame (`frame: Frame::Inline/Heap`, `ScopeInfo::frame_size`
  slots), a dedicated unit accumulator and counter, and a pile of
  per-activation state vectors (`try_stack`, `pending`, `for_of_stack`,
  `destructure_stack`, `for_in_stack`, `async_for_of_stack`, `class_stack`,
  the completion/list stacks, the env stack).
- **JIT**: a **private working buffer** (`run_jit_body`'s `work`) plus a
  Cranelift stack pointer, a per-run `JitCallContext` (~40 fields) rebuilt
  every call, and a pooled `Vm` handed out by value-then-box.

The only reason a compiled body does not reuse the caller's frame is that
these two regions disagree. So a nested call must swap to a fresh `Vm` (and
its fresh private buffer) and re-derive everything the ctx carries. That is
the ~126–255 ns, and it is why the leaf-inline path (which *does* run the
callee on the caller's region) is 20× cheaper — it is the one case where a
frame is shared.

The object/construct cost is the same fact one level down: a construct adds
receiver creation and a vector-form arg list *because* the arg list is not
in the frame.

## 3. The design

**One described frame region, carved from the shared value stack, addressed
identically by the interpreter and the JIT.**

1. **Frame descriptor.** A frame header on the value stack: `{ callee,
   this, argc, flags, saved_base, saved_ip, saved_scratch }` — the
   `JitFrameLayout`/`FrameDescriptor` shape. `argc` in the descriptor retires
   the `Vm::args` vector and the Cow/truncate adaptor (SM's `numActualArgs`,
   V8's removed adaptor). `this` and args live at fixed offsets above the
   header (`offsetOfThis`, `offsetOfActualArg`).
2. **Frames are carved from `Vm::stack`, not allocated.** A call pushes a
   header + args region and sets the base; a return pops to `saved_base`. The
   `frame: Frame::Inline(buf)` fixed buffer becomes "the current frame's slot
   region" addressed by `base + slot` — exactly the flat segment the leaf lane
   already uses (`leaf_frame_base`). The private JIT buffer disappears: the
   JIT's operand stack **is** `vm.stack` (which `DISPATCH_DEOPT` already
   assumes, and which removes the ctx rebuild).
3. **Per-activation state becomes frame-bounded watermarks.** The `Vm`'s
   state stacks stay shared, but each frame records the watermarks at entry
   and truncates on exit — the discipline `save_scratch`/`restore_scratch`
   already implements for the nested lanes. A frame owns `try_stack[..]`,
   `env_stack[..]`, the completion/list stacks, etc. up to its watermark. This
   is the hard half of the plan and the reason the current design allocates a
   `Vm` per call; it is where a stage must be careful.
4. **The context is the frame, not a struct.** `JitCallContext` stops being
   rebuilt per call: its immutable per-run inputs (global, cells, realm) move
   to the `Vm`/agent, and its per-frame inputs (entry, working base, frame
   slot base) are read from the frame descriptor. A nested compiled call
   inherits the running ctx. The layout follows SM's `Instance`: the hot
   fields first, at small fixed offsets (an asserted compact-offset head), and
   whatever state the compiled code needs per activation — not reallocated per
   call.
5. **Both engines emit against this frame.** The interpreter already does.
   The JIT's `emit_call` leaf-inline (`emit_leaf_call_tail` — the callee on
   the caller's frame region) becomes the *general* call lowering: a non-leaf
   callee is the same push plus a recursive body, not a new `Vm`. The
   construct adds the receiver creation and the base-return rule to the same
   protocol.
6. **Deopt is unchanged and gets cheaper.** `DISPATCH_DEOPT` (spill the
   working region into `vm.stack`, set `vm.ip`, resume the interpreter) is
   already frame-relative; with one stack it works uniformly for nested
   frames, and no translation stream is needed because the compiled frame
   *is* the interpreter frame (V8's translation machinery exists only because
   its optimized frame is not).

## 4. Stages

Each stage is independently landable, probe-first, both engines together,
full gate before landing. A perf change that costs a fixture is reverted.

| stage | content | gate / target |
|---|---|---|
| **C0** | Frame descriptor + carve the *certified* JIT frame from `vm.stack`, retiring the private buffer. The JIT's working stack becomes `vm.stack` for a lifted body too. | equivalence on the corpus; no row regress. Enables the rest. |
| **C1** | Re-entrant frames on one `Vm`: generalize `emit_leaf_call_tail` to a non-leaf callee that pushes its own frame region. Per-frame watermarks for the state stacks. | `non-leaf call` row, `recursive_fib`; both test262 areas. |
| **C2** | Construct protocol: receiver creation + base-return rule folded into the frame push (opportunity 4). | `construct_churn`, the bare-construct probe (~128→~25 ns). |
| **C3** | The interpreter's call path uses the same frame push (no `take_vm`), so JIT↔interpreter calls are frame-compatible. | interpreter call rows (7–10× the JIT). |
| **C4** | Args in the frame: retire `Vm::args` and the adaptor truncation; `this`/args read by offset. | arg-heavy rows; no parity change. |
| **C5** | Exception/stack-walk/`Error.stack` and GC tracing over the frame chain (the frame is traced from the `Vm`; nested frames are on `vm.stack` already). | `try_catch_loop`, stack-trace fixtures, `--gc-stress`. |

C0 is the substrate; C1 is the first measured win and the point of the whole
plan; C2 is the sibling win; C3/C4 remove the residual swap; C5 is the
correctness net that must exist before C1 ships under `--gc-stress`.

**Stage C1 detail (the first real landing).** Reproduce the leaf lane's
protocol for a non-leaf callee: at a compiled `Call` site whose callee is a
certified body (from the call-site cell, not re-derived), emit the frame
push (header + args from the working stack), set the base, and enter the
callee's compiled entry with the *same* ctx and `frame`/`vm` pointers; the
callee's `Return` pops to `saved_base` and leaves its result in the caller's
result register. The leaf lane's `emit_leaf_call_tail` is the template; the
new part is the recursive entry/exit and the per-frame watermarks. Probe:
the `non-leaf call` row and `recursive_fib`, isolated A/B, plus
`--gc-stress` and the six sweeps.

**C1 reconciliation with the landed certified-callee lane (2026-10-05).** C0
removed the private JIT frame/working buffer, and the certified-callee lane
(`runtime::jit::certified_call_inline`, the slag-jit skill's §21) already runs a
*different* certified body as a nested frame on the caller's `vm.stack` region,
with its own `JitCallContext`, skipping
`do_call_fast`/`ordinary_call`/`run_compiled_body`/`run_jit_body`. So the C1
row is already half done at *runtime* — the frame push exists. What C1 adds is
the **emit-side** recognition, so a certified non-leaf call site stops paying
`call_slow`'s per-call re-derivation.

Today a certified non-leaf site (`bench` -> `mid`, the `non-leaf call` row) is
reached like this: the emit-side leaf probe refuses `mid` (it has a call step),
the site takes `call_slow`, which tries the self path then
`certified_call_inline`. Every call re-derives what is a pure function of the
callee: the `Value` decode, the `ecma_functions` record lookup,
`CompiledBody::certified_callee_eligible()` (a step scan), the `lookup_info`
consult, and `Vm::global_reads_are_unshadowed(environment, ident_names)` (a
chain walk). Baseline (2026-10-05): `non-leaf call` jit 7.41 ms / 100k iters
(~74 ns/call); the leaf-call floor is ~6–7 ns.

**C1 design: a certified verdict in the callee-keyed call-site record.**
Extend `LeafCallRecord` with a `CertifiedInlineInfo` the probe fills when the
leaf verdict refuses `{ entry, frame_size, arity, stack_usage, tdz_mask,
this_slot, fill_ok, strict, globals_unshadowed, global_value, realm_ptr,
has_call_apply, has_call_intrinsic, environment }` plus a `certified: bool`
discriminator. The record's existing identity / `epoch` / `code_gen` gate guards
reuse exactly as the leaf verdict does.

**C1 emit-side lane (the measured win).** In `emit_call`, after the leaf gate
misses, consult the record's certified verdict. On a hit, call a new helper
`Helper::CertifiedCall` — a `certified_call_inline` keyed by the record, so it
does no decode / HashMap lookup / eligibility scan / `lookup_info` / globals
walk, only the frame fill, `save_scratch`, env swap, nested-ctx build and entry
call — and land the result through `emit_leaf_result_tail`. The entry call stays
inside the helper (the nested `JitCallContext` is a Rust local with a
synchronous lifetime), so the split-ctx lifetime problem is avoided; the win is
the re-derivation, not the FFI hop.

**C1 slices** (each independently landable, probe-first):
- **C1a** — extend `LeafCallRecord` with `CertifiedInlineInfo` and fill it in
  `leaf_call_probe`; no behavior change (the emit lane ignores it). Measure
  the probe's added cost is below noise.
- **C1b** — add `Helper::CertifiedCall` + the emit lane gated on the record's
  certified verdict; land the result through `emit_leaf_result_tail`. Probe the
  `non-leaf call` row and `recursive_fib`.
- **C1c** — the per-frame watermark audit: nested `try`/`finally`, for-of,
  destructure, generator/async bodies called through the lane, under
  `--gc-stress`/`--nursery-stress` and the six sweeps.

**C1 traps.** (1) The record is callee-keyed and its `entry` outlives a run, so
the certified info MUST carry the code generation like the leaf info (a stale
entry after an eviction is a jump into freed code) and be cleared by
`gc_safepoint`. (2) `environment` is a `GcAny` held across calls while the
record lives on the `Agent` and is not traced — the same sweeping-collection
clear that protects the leaf identity must clear it. (3) The lane keeps
`certified_call_inline`'s gate exactly, the cross-realm unmapped-`arguments`
refusal in particular, or the arguments object is built in the wrong realm.
(4) `global_value`/`realm_ptr` are raw bits/pointers — do not add a traced
handle to the record.

**Status (2026-10-05): C1a + C1b + C1c landed.** The callee-keyed record now
carries the `CertifiedInlineInfo` the probe fills, and `emit_call` offers a leaf
miss to `Helper::CertifiedCall` (a `certified_call_inline` keyed by the record)
before the interpreter funnel. Measured `non-leaf call` jit 7.41 → ~5.5 ms
(~1.35×) and `recursive_fib` unchanged; stable across three runs. Two
correctness items the gates forced, both in the landed change: the cached verdict
verifies the callee's function `id`, because the box-address identity is unsound
for a POSITIVE cache (a swept callee's box can be recycled by another closure, so
the address matches while the descriptor is another body's;
`Iterator/zipKeyed/basic-longest` caught it); and **C1c lifts C1b's
`can_inline_leaf` refusal** so the lane runs inside the caller's `try`/for-of/
block/destructure instead of falling to the funnel.

The isolation C1c actually needs is narrow. A lane callee is always a compiled
body, and the compiled model runs the statement-list wrappers
(`ListBegin`/`ListEnd`) and the completion saves as no-ops, while the lane gate
(`has_shared_vm_step_hazard`) excludes every producer of the other control
stacks — so the only shared stack a nested run can grow is `env_stack`. The lane
thus save/restores `env_stack` and leaves the caller's other stacks in place: the
callee cannot touch them, and its throw reaches the caller's handler through the
returned pending error. `has_control_state` is therefore just
`env_stack.len() > 1` (the save is the cold `save_control_state` path for a
caller deeper than its body env; the at-rest fast path only resets the shared
`env_stack`), and the per-activation state lives on the traced `Vm`
(`Vm::saved_control`) so a collection during the nested run keeps it alive.

**The measured C1c trap was code size, not the fast path.** The `SavedControl`
push/restore blocks inlined into `save_scratch`/`restore_scratch` made them too
large to inline, and every call — branch taken or not — paid for it (`non-leaf
call` 5.5 → 6.3 ms, `recursive_fib` 54 → 87 ms). Outlining both into
`#[cold] #[inline(never)]` helpers restored the rows (`non-leaf call` ~6.1 ms,
`recursive_fib` ~56 ms; the residual is the lane's env swap). Verified: normal
sweep at baseline (48622 / 48464 pass / 0 fail / 158 skip / 0 crash / 0 hang),
clippy `-D warnings`, `cargo test -p jit --lib` 261 green (the two
`…_does_not_drift` tests plus a new caller-control-state differential,
`installed_jit_certified_callee_inside_caller_control_state_matches_the_interpreter`).
The `--gc-stress`/`--gc-verify`/workspace/wasm re-run for C1c was deferred by the
operator.

**Stage C2 detail (the construct protocol on the frame).** C2 is the construct
mirror of C1. Today a compiled `new C()` emits one `Helper::Construct` FFI
(`crates/jit/src/compiler.rs`, the `Construct` arm: pop the callee, call the
helper with the caller's `sp`). `step_construct_impl`
(`crates/runtime/src/ir.rs`) pops the argument boundary and runs the certified
base-constructor LEAF path — `run_leaf_construct` (`construct_this_object` + the
base-return rule, and a `try_shared_construct_leaf` run on the caller's ctx when
the callee is an env-free leaf) — when `Vm::can_inline_leaf` and the leaf
cache's `construct_inline` verdict hold; otherwise it falls to
`function::construct` → `ordinary_construct`'s certified path, which pushes an
`ExecutionContext`, `take_vm`s a pooled `Vm`, `setup_frame`s, and `vm.start`s the
body. Two costs remain: (1) a **non-leaf** certified base constructor (a body
with a call) never takes the leaf path, so it pays the pooled-`Vm` take/reset +
context push + `run_compiled_body` per construct; (2) the **leaf** path
re-derives per construct what is a pure function of the callee (the `leaf_lookup`
HashMap probe + `construct_inline` verdict + the `lookup_info` consult inside
`try_shared_construct_leaf`).

**C2a — a non-leaf certified base constructor on the caller's frame
(runtime-only).** Generalize the certified-callee lane (`certified_call_inline`)
to construct: when the `shared` ctx + `sp` are present (a compiled caller) and
the callee is a certified base constructor, carve the nested frame from
`vm.stack` exactly as the call lane does, set the `this` slot to
`construct_this_object(agent, new_target)`, set `Vm::current_new_target`, run the
callee's compiled entry on the caller's ctx, and apply the base-return rule (an
object/function return wins, else the receiver). Gate: `constructor_kind ==
Base`, no instance `fields`/`private_methods` (the `ordinary_construct` certified
gate), capture-free (`context_names` empty), single realm, and the C1 lane's
`has_shared_vm_step_hazard` exclusion. `new_target` is the callee for a direct
`new C()`; a `Reflect.construct`/subclass new_target differs and is refused
(the lane uses the callee as the receiver's prototype source).

**Hazards (construct adds over call).** The base-return rule (a returned object
overrides the receiver); `new.target` — the body's `Step::NewTarget` reads
`Vm::current_new_target`, which the call lane sets to `None` and the construct
lane must set to the callee; derived constructors / `super()` (the this-TDZ
before super: refused by the base-kind gate); class instance fields/private
methods (`initialize_instance_elements`: refused, they keep `ordinary_construct`);
a sloppy body's mapped `arguments` (refused like the call lane; the unmapped
form reads `Vm::call_args`).

**C2b — the construct-site verdict cache (emit-side): probed and declined
(2026-10-05).** A `Construct` site emits one `Helper::Construct` FFI with no
site record; C2b would add a construct-site record (`ConstructInlineInfo`, keyed
by the callee's `id` + site, with the leaf record's identity/`code_gen`/`epoch`
gate) and a `Helper::CertifiedConstruct` that skips the per-construct
re-derivation — the construct mirror of C1a/C1b. The probe says it is not worth
it. The one *re-derivation* in the construct lane was a **double eligibility
scan**: `certified_construct_eligible` scans the body, then `certified_lane_inline`
re-ran `certified_callee_eligible` on the same body. That was removed for
constructs (C2b-lite, non-leaf 128 → ~111 ms / 500k). What remains of the ~50
ns/construct residual (non-leaf ~111 ms vs leaf ~87 ms) is *not* the
re-derivation a record can cache:

- `CertifiedInlineInfo` carries only the *verdict* (entry, frame layout,
  `globals_unshadowed`, `has_call_*`, `tdz_mask`, `callee_id`) — **not** the
  `body`/`environment`/`realm`. So the lane's resolve
  (`agent.ecma_functions.get(&id)` + `data.ir.clone()`) runs even on the cached
  path. C1b has the same property; its win came from skipping the
  `certified_callee_eligible` scan and the `global_reads_are_unshadowed` walk, not
  the record lookup.
- The lane's `save_scratch`/`env_stack` swap/`restore_scratch` is required (the
  callee reads its capture environment) and cannot be cached away.
- `lookup_info` is a cached-pointer load for a hot body (`ir.jit_info`), not a
  per-call compile consult.

So the record could only skip `lookup_info` (negligible) and the globals walk
(empty for a constructor with no global reads). The C2a probe does not show the
re-derivation as the residual, so C2b is declined. Revisit only if a construct
row appears where the `ecma_functions` lookup or the env swap measurably
dominates.

Probes: a non-leaf certified base constructor row and the bare-construct probe
(`--jit-bench`/the corpus), A/B; `construct_churn` and `recursive_fib` must not
regress. C2a landed (the measured win); C2b was probed and declined (above).

**Status (2026-10-05): C2a landed.** `certified_call_inline` is now
`certified_lane_inline(ctx, callee, this, args, argc, cached, construct)` — one
frame-carve/run/restore core shared by the call and construct lanes — and
`step_construct_impl` runs a certified base constructor as a nested frame on the
caller's `Vm` (the construct args rooted on `vm.stack` for the window). Measured
on a non-leaf base constructor (a body that calls a function), 500k constructs:
**259 → 128 ms (~2×)**; a leaf constructor (already on `run_leaf_construct`) and
`recursive_fib` are unchanged. `construct_churn` is a leaf-constructor row, so
it is the no-regress guard, not the win. Gates: normal sweep at baseline
(48622 / 48464 pass / 0 fail / 158 skip / 0 crash / 0 hang); clippy
`-D warnings` clean; `cargo test -p jit --lib` 262 (the new
`installed_jit_a_certified_construct_matches_the_interpreter` covers a non-leaf
constructor, both base-return rule arms, a base class, and the
derived/instance-field fallbacks); `cargo test -p runtime --lib` 994; a
self-checking non-leaf construct under `--gc-stress`/`--nursery-stress`; the CLI
benches 12/12 `ok=true`. **C2b** (the construct-site verdict cache) was probed
and declined; C2b-lite removed the double eligibility scan (128 → ~111 ms).

**Stage C3 detail (one frame for the interpreter call path).** C0–C2 gave the
JIT lane a nested frame on the caller's `Vm`; the interpreter still allocates a
frame per call. A non-leaf call from the interpreter is *withdrawn*
(`fast_call_core` records `Vm::pending_call` and returns; the driver performs it)
and `complete_call` → `crate::function::call_inner` → `ordinary_call` (the
certified path) → `run_compiled_body`, which `take_vm`s a POOLED `Vm` (a full
reset), pushes an `ExecutionContext`, `setup_certified_frame`s, runs
(`run_jit_body` for compiled code, `vm.start` for a below-threshold body), and
`return_vm`s. So the interpreter pays a whole second `Vm` per call — the frame
the JIT lane does not.

**Mechanism.** `complete_call` owns `&mut Vm` (the driver's), so a certified
callee can run as a nested frame on the SAME `Vm` — the shape
`certified_lane_inline`/`self_call_inline` use, but driven from `ir.rs` and
serving the interpreter's run (`run_jit_body` on `self` for compiled code, a
nested `vm.start` for a below-threshold body). Isolation is
`save_scratch`/`restore_scratch` (`ip`/`acc`/the shared cursors) plus the
per-activation control state; the callee's body context comes from
`new_body_context`, the frame from `setup_certified_frame`, and the realm/env
from the callee's record (the nested ctx `run_jit_body` builds carries its
`global_object`/cells). The result lands at the call site (the withdrawal's
completion contract, unchanged).

**Why it is the hard half (the plan's §3 warning).** The nested interp run is
NOT gate-excluded the way the JIT lane's callee is: a certified body may itself
contain a `try`/for-of/destructure/`yield`, so (a) the control-state isolation
must be the FULL watermark (`Vm::saved_control`, C1c), not C1c's narrowed
`env_stack`-only fast path — the callee pushes `try_stack`/`for_of_stack`/
`env_stack` and a throw/return must unwind them against the callee's frame, not
the caller's; (b) the nested `run_inner` pushes its own `ExecutionContext`, and
`dispose_env_resources` runs on the body env on every exit (return and abrupt).

**And the frame is `vm.frame`.** The interpreter addresses its activation
through the single `Vm::frame` (`Frame::Inline([Value; 8])` or `Frame::Heap`):
`frame_get`/`frame_get_mut`, `setup_certified_frame`/`setup_frame` and
`run_jit_body` all read/write it directly. So a nested call cannot reuse the JIT
lane's `nested_frame` carve as-is — `run_jit_body` `debug_assert!`s
`nested_frame.is_none()` and reads `vm.frame`, and `setup_frame` writes
`vm.frame`. The caller's frame must therefore be SAVED across the nested run,
and saved on the traced `Vm` (a `Vec<Frame>` stack, like `saved_control`): a
Rust-local `Frame::Heap(Vec)` copy is invisible to the collector and its slots
would be swept. That frame save/restore, not the call bookkeeping, is the
substrate C3a lays. The alternative (route the interpreter's frame access
through a base+offset cursor so nested frames live in `vm.stack`, as `run_jit_body`
already does for the working region) is the fuller "one frame" refactor of §3
and is deferred.

**Slices** (each independently landable; the corpus `jitless` column + the
sweeps are the gate):
- **C3a** — the certified nested frame for the withdrawn interpreter call:
  `complete_call` runs a certified `PendingCall::Function` on `self` instead of
  `call_inner`/`run_compiled_body`. The args were left at `arg_start` by the
  withdrawal, so the callee's frame is built from that `self.stack` slice (the
  C2 construct lane's borrow shape: read through the raw frame pointer, not a
  held slice); `save_scratch` covers the nested `start`. Probe the `--bench`
  interpreter call rows (`function calls`, `method_call`, `construct churn`) and
  the corpus `jitless` column (`function calls`, `direct_leaf`, `recursive_fib`,
  `construct_churn`).
- **C3b** — the in-place non-leaf arm: `fast_call_core` runs a certified
  NON-leaf callee in place (today only the leaf lane inlines; a certified
  non-leaf withdraws). Shares C3a's nested-frame primitive.
- **C3c** — the interpreter tail path (`tail_call_shared`) onto the same frame
  push (the interpreter's TCO already loops on one `Vm`, so the frame rebind is
  the natural extension).

**Open decision.** Whether C3a absorbs the below-threshold (interpreted) callee
into the nested `start` (one uniform frame primitive) or only the compiled case
first (smaller diff; the interpreted callee keeps `take_vm`). The interpreter-row
target wants both: Cut 69's compile threshold means many one-shot bodies never
compile, so the interpreted nested `start` is where most of the row lives.

**C3a probe (2026-10-05): negative — do not land as-is; the premise does not
hold.** A full C3a was implemented and measured (uniform nested frame for the
withdrawn interpreter call: `save_scratch`/`saved_frames` park of the caller
frame + full control watermark, `new_body_context` + `setup_certified_frame` +
`run_compiled_body_on` on the driver's `Vm`; the `run_compiled_body` core was
split out so both the pooled path and the nested path share it). It is correct
(clippy clean; `cargo test -p runtime --lib` 994, `-p jit --lib` 262), but it is
not faster. The measured result is a small **regression** on the interpreter and
neutral elsewhere:

- corpus `--corpus tools/corpus/workloads/calls --jitless` (min of repeated
  runs, the stable figure): `recursive_fib` ~587ms (base) → ~660-695ms (C3a),
  i.e. ~10-15% slower; `method_call`/`construct_churn`/`apply_call`/
  `closure_capture`/`direct_leaf` are flat (within ~2-3%).
- corpus default (JIT on), same loads, min-of-5: all six flat (e.g.
  `method_call` 110.8 vs 113.9, `recursive_fib` 55.0 vs 55.5).
- `--bench` interpreter rows, min-of-8: `function calls` 29.68 → 28.64 (≈+3%,
  noise), `closure capture`/`construct churn` flat.

**Why.** The premise — "the interpreter pays a whole second `Vm` per call" —
was already not the bottleneck. The pool reuses Vms (`take_vm`/`return_vm` is a
`Box` pop/push + `reset`, and only ~recursion-depth Vms are ever allocated), so
`reset`'s ~20 `Vec::clear`s are cheaper than C3a's *inherent* bookkeeping: the
caller's frame must be parked on the traced Vm and the per-activation scratch +
control watermark saved and restored around every call. Two trim probes did not
close it: a full `clear_control_state` (control-free caller) vs a one-line
`list_stack.clear()` (a `nested_call_eligible`/`touches_semantic_control` split)
moved `recursive_fib` from ~-15% to ~-10% at best, and the residual (scratch
save/restore + frame park + gate scan) still exceeds `reset`. The win this arc
is after is the *fuller* "one frame" design of §3 — per-frame scratch/control
with watermarks, so there is nothing to save per call — not the C3a half-step,
which pays a save/restore to avoid an allocation that was never the cost.
Implementation preserved at `scratch/c3a/` (gitignored) for re-landing if the
substrate is wanted for C3b/C3c.

**Stage C3 detail (the interpreter call path onto the §3 frame).** The C3a
probe rules out the "save less" reading of C3. §3's win is not a cheaper save;
it is the frame *becoming* the activation, so a call is a frame push and nothing
per-activation is copied. A control-free caller is the proof: for
`recursive_fib` the C3a control save was already narrowed to a single
`list_stack.clear()`, and the residual ~10% was the frame park + the scratch
`mem::replace` + the per-call gate. Those are exactly §3 points 2 and 2/6, so C3
adopts the three substrates together rather than the C3a shortcut.

**Substrate 1 — frame carve (point 2).** A non-leaf interpreter callee's frame
slots become a `vm.stack` segment addressed by `leaf_frame_base`, the mechanism
the leaf lane already uses (`Vm::frame_get`/`frame_get_mut`, `run_leaf_body`
pushes the slots and sets the base). The caller's `vm.frame` is then untouched —
the frame park (`saved_frames` + `mem::replace` + push/pop) disappears. Nested
calls stack naturally: `leaf_frame_base`/`leaf_frame_offset` are saved as two
`usize`s per activation (the native stack holds them) and truncated on exit. The
one JIT-visible change: `run_jit_body` must derive `frame_ptr` from
`vm.stack[leaf_frame_base..]` when `leaf_frame_base` is `Some` (it currently
reads `vm.frame` unconditionally), so a *compiled* callee runs on the same carve;
gated on `leaf_frame_base.is_some()`, the top-level path is unchanged.

**Substrate 2 — per-frame scratch (points 2/6).** The per-activation registers
(`ip`, `acc`, `loop_counter`, `loop_num`, `builder`, `completion`,
`completion_is_empty`, `switch_disc`, `switch_disc_set`, `chain_short`) move into
the frame region; the interpreter addresses them at `frame_base + k`. This is
what removes `save_scratch`/`restore_scratch` (~40 ops/call). It is the invasive
part — every `self.ip`/`self.acc`/… in `run_inner_inner` (and the JIT helpers
that share them) becomes a frame access. Slice it behind accessors
(`Vm::ip()`/`Vm::set_ip()`, …) that read the frame when a nested carve is active
and the existing field at the top level, so the loop migrates incrementally and
the top level pays nothing.

**Substrate 3 — control watermarks (point 3).** The semantically scanned state
stacks gain a per-activation base, zero at the top level and at every pooled run:
`try_base`, `pending_base`, `for_in_base`, `for_of_base`, `async_for_of_base`,
`destructure_base`, `yield_star_base` (plain `usize`s; not GC). Every *read* is
bounded to `[base..]`, which is what makes a shared stack equivalent to the fresh
`Vm` the pooled path hands out; a frame exit truncates each to its base. A
watermark is sound for every one of these because the callee only ever reads its
own LIFO top: the audited read sites are ~20, all `.last()`/`.iter()` on the
active tail — `route_step_error`'s `covered` scan (`ir.rs:6847`), `ForInNext`
(`:9018`), `AsyncForOfNext` (`:9127`), the `yield*` sites (`:9687`–`:10098`),
`for_of_advance` (`:14185`/`:14212`/`:14217`), `control_transfer`'s
`pending.last()` (`:14483`), and the `find_finally_frame`/`throw_machinery`
`try_stack` scans (`:14556`/`:14591`); the `list_stack`/`completion_stack` are
LIFO-only and need no base (truncate on exit suffices). The bulk of the ~160
`*_stack` references are push/pop/clear/trace/reset/save/restore, not reads.
With bases at 0 this refactor is a behavior-preserving no-op for every path that
exists today.

**Slice sequence** (each probe-first, full gate). **C3.1** — watermark bases +
bounded reads (substrate 3), bases 0 at the top level; a no-op refactor plus the
nested-call switch from `save_control_state` to `set-bases`/`truncate-bases`.
Probe: a caller *with* a live `try`/for-of around a certified call (the case the
C3a clear paid for), `--gc-stress`/`--nursery-stress`, the six sweeps. **C3.2** —
frame carve for the nested interpreter callee via `leaf_frame_base` (substrate 1)
+ the `run_jit_body` frame derivation; probe `recursive_fib` and the corpus
`jitless` column. **C3.3** — per-frame scratch accessors (substrate 2), migrating
`run_inner_inner` incrementally; probe the same rows. **C3.4** — retire `take_vm`
from `complete_call` entirely once 1–3 hold, and fold C3b (the in-place non-leaf
`fast_call_core` arm) onto the same carve. The `nested_call_eligible` gate's step
scans (`nested_call_eligible`/`touches_semantic_control`) should be cached on the
`CompiledBody` (a `Cell`) so C3.3's residual gate cost is one load.

**C3.2 result (2026-10-05): landed, correct, marginal on its own — keep as the
carve substrate, C3.3 is the lever.** The nested interpreter call now carves
its frame as a flat `vm.stack` segment and installs it as `leaf_frame_base`;
`run_jit_body` derives `frame_ptr` from that base when set (the top level still
reads `vm.frame`), so a compiled callee shares the carve. The one real hazard
found: `Vm::frame_get` checks `leaf_frame_base` before `nested_frame`, so a JIT
lane running *inside* a carve read the wrong frame through helpers — fixed by
having the lanes take `leaf_frame_base` off for their run
(`Vm::install_nested_frame`/`restore_nested_frame`; two `#[ignore]`-worthy
regressions, `installed_jit_a_certified_callee_matches_the_interpreter` and
`installed_jit_self_call_hazards_match_the_interpreter`, caught it). Gate:
normal sweep at baseline 48622 / 48464 / 0 / 158 / 0 / 0; clippy `-D warnings`;
`cargo test -p runtime --lib` 994 and `-p jit --lib` 262; `--bench
--gc-stress`/`--nursery-stress` 12/12 `ok=true`.

Perf was marginal at first, because the carve removed only the *frame park*:
the scratch `save_scratch`/`restore_scratch` and the per-call gate scans
remained. **Caching the gate** — `nested_call_eligible`/
`touches_semantic_control` are pure functions of the step list, so they became a
`Cell<Option<(bool, bool)>>` on `CompiledBody` — flipped it to a small net win
(the two scans were ~5.5% of `recursive_fib`). Final, min-of-N, same machine:

- corpus `jitless`: `closure capture` ~158 → ~146 ms (~8%), `apply_call` ~116 →
  ~111 (~4.6%), `direct_leaf` ~80 → ~78 (~3.3%), `method_call` ~200 → ~197
  (~1.4%), `construct_churn` flat, `recursive_fib` ~589 → ~582 (parity; it was
  ~646 / −6% before the gate cache).
- corpus default (JIT): all six flat (no regression).
- `--bench` interpreter rows: flat within noise.

So C3.2+gate-cache is a small, broad win on the interpreter and neutral on the
JIT. What remains is the *scratch* (`save_scratch`/`restore_scratch` ~ per call)
and `is_control_free` (C3.1/C3.3): the accessor-indirection route (C3.3) adds a
load per `ip`/`acc` access across ~300 sites, which for an ip-heavy loop is
likely to exceed the ~58 ops/call it saves — so C3.3 should be attempted only if
a per-access-cost-free framing is found (e.g. the dispatch loop keeping the
registers in Rust locals and syncing only around helper calls), not the
`Vec`/pointer accessor route.

**C3.1 result (2026-10-05): probed and declined — the watermark costs more than
it saves.** A full watermark landing (a `ControlBase` of 13 stack lengths, every
error/abrupt-path read bounded to `[base..]`, the close loops stopping at the
base, the carve setting the base at entry and truncating on exit, and
`is_control_free`/`save_scratch_full`/`clear_control_state` retired) was
implemented and measured: correct (clippy, `994`/`262`, the six sweeps at
baseline) but **~2.6% slower on `recursive_fib`** than C3.2+gate-cache
(`--corpus …/calls --jitless`, median-of-6: ~620 ms vs ~604 ms). The reason is
structural: `ControlBase::capture` + `truncate` touch 13 stacks (~26 ops plus a
104-byte struct copy each way) — *more* than `is_control_free`'s 23 field reads
plus the one-line clear it replaced. So `is_control_free` was already the cheap
decision, and the take only ran for callers with live control state (rare);
watermarks cannot beat it. Reverted; C3.1 as a *cost* lever is closed. Its one
lasting value would be admitting callers-with-control-state to the carve without
a take, which the corpus does not exercise — not worth the 13-stack bookkeeping
on every call.

**C3.3 detail (the real lever, and it is a rewrite, not a rename).** The
residual per-call cost after C3.2+gate-cache is `save_scratch`/`restore_scratch`:
the nested run copies the 10 per-activation registers (plus the ~60-byte
`Builder`) off the `Vm` and writes fresh defaults, then copies them back — ~60–75
word-ops/call, which the pooled path gets for free from `reset` on a throwaway
`Vm`. The accessor-indirection route (make `ip`/`acc`/… fields of a
`Vec<FrameScratch>` addressed through a pointer) replaces that copy with a *load
per access*, and the dispatch loop accesses `ip` (and often `acc`) several times
per step, so on an ip-heavy loop it would cost more than it saves. The only
per-access-cost-free framing is to keep the registers in **Rust locals inside
`run_inner_inner`** and write them back to `self` only at the boundaries that
need them:

- Hoist `ip`, `acc`, `loop_counter`, `loop_num`, `completion`,
  `completion_is_empty`, `switch_disc`, `switch_disc_set`, `chain_short` into
  loop locals; the step match reads/writes the locals (register-resident).
- Sync **to** `self` before any call that reads the register: `set_site` (the
  error span), `control_transfer`/`throw_machinery`/`route_step_error` (they read
  `self.ip` to pick a handler), `close_*`, `start_scope_disposal`, and every
  `Step` helper that takes `&mut self` and may re-enter (`for_of_advance`,
  `destructure_*`, `builder_*`, the `Call*` family). Sync **from** `self` after a
  helper that can change `ip` (those that set it, e.g. `for_of_next`,
  `tail_*`, a resumed disposal).
- The `Builder` is the one value that is not register-sized: keep it on `self`
  (its owner/cursor check already scopes it), so only `ip`/`acc`/the scalars
  need the local treatment.
- A nested call then costs **no** register save/restore at all: the callee's own
  `run_inner_inner` has its own locals, and the caller's stay in its frames — the
  `save_scratch`/`restore_scratch` block is deleted from the carve.

The work is mechanical but wide (the ~300 `self.ip`/`self.acc`/… sites in
`run_inner_inner` and the shared step helpers), and the sync points are the trap:
a miss reads a stale `ip` and the sweep will not always catch it (an off-by-one
handler index often still resolves to *a* handler). Stage it: (1) hoist `ip` and
the scalar flags only, sync at the existing `self.ip =`/`self.ip +=` sites plus
the helper boundaries, and drop the `save_scratch` register save for those; (2)
batch it behind the six sweeps plus `--gc-stress`/`--nursery-stress` and the
`--bench` rows; (3) add `acc` and the loop counters the same way. Probe after each
step — this is the one C3 slice that can move `recursive_fib` below the pooled
floor (`function calls` ~1.4× the leaf lane is the ceiling).

**C3.3 result (2026-10-05): declined — the register block is already at noise
after C3.2 (~0–2% ceiling, measured); do not attempt the locals rewrite or the
per-body clobber mask.** Reading the code settled the framing the detail left
optimistic:

- The step path never touches `self.acc` at all (only the register/leaf executor
  reads it — the leaf lane states the invariant at `ir.rs:11707`), so `acc`'s
  save/restore is the one *unconditionally* removable field — and it is a single
  field (three word-ops).
- Every other register is entangled with a helper the step arms call, so a locals
  hoist cannot delete its save/restore without either syncing around the helper
  (re-introducing the per-call cost) or inlining it: `loop_counter`/`loop_num`
  via `fast_loop_bind`/`fast_loop_test`/`fast_loop_inc`/`fast_loop_store` (called
  from `Step::FastLoop*`), `completion` via `start_scope_disposal`, and `ip` via
  `route_step_error`/`throw_machinery` (the error path reads `self.ip` to pick
  the handler, so a local `ip` would need a per-step `self.ip = ip` store —
  negating the hoist for the hottest register).
- `completion` and `switch_disc` are `Value`s; a Rust local is rooted only by the
  conservative stack scan (see the `slag-gc-rooting` trap), so hoisting them is
  a soundness risk under `--gc-stress`.

So the cleanly-hoistable set is three bools (`completion_is_empty`,
`switch_disc_set`, `chain_short`) plus at most `switch_disc` — under a fifth of
the block — and **the block is not where the call time goes**: a throwaway build
that skipped seven of the ten register default/save/restore slots (keeping `ip`,
`completion`, `completion_is_empty`) measured a median *within noise* on jitless
`recursive_fib` (interleaved min 554→543 ms of ~560; a lone ~5% reading did not
reproduce). The carve's per-call register handling is already down to noise
after C3.2, and there is no larger move hiding here.

The "per-body clobber mask" is the only sound way to shave what remains, and it
is worth ~0–2% at best: default, save, and restore only the registers the
callee's steps can *read or write* — `ip` and the fall-off-end `completion`/
`completion_is_empty` always; `acc`/`loop_*`/`builder` only with a `Step::Leaf`
or `FastLoop*`/`Builder*` step; `switch_*`/`chain_short` only with their steps.
Skipping the *default* write too is essential: keeping the default but dropping
the restore lets the callee zero the caller's live `loop_counter` and
`recursive_fib` hangs. Given the payoff, do not attempt either this or the
locals rewrite — the interpreter call frame is at its floor.

**Stage C0 detail (the substrate).** C0 makes the compiled body's working
region *be* `vm.stack`, so the frame and the operand stack are one described
region both engines address. Today `run_jit_body` hands the entry a
`frame_ptr` into the fixed `Frame` enum and a `work_ptr` into a private
buffer (`crates/runtime/src/jit.rs:6363`, rooted by `with_jit_run` through
`jit_roots`); `run_jit_leaf` does the same. The only reason for the private
buffer is the reallocation hazard: a helper gets `&mut Vm` and may
push/realloc `vm.stack`, so a raw pointer into `vm.stack` held across a
helper call would dangle. The frame slots already prove the pattern is
viable — the interpreter leaf addresses a flat `vm.stack` segment through
`leaf_frame_base` (`Vm::frame_get`, `crates/runtime/src/ir.rs:4037`), and
`DISPATCH_DEOPT` already spills the working region into `vm.stack`.

The one mechanism C0 must settle first is how the compiled code survives a
realloc. This is the decision the stage turns on:

1. **Stable region (recommended).** Cap `vm.stack` so it never reallocates and
   a baked pointer stays valid, as the entry ABI assumes today. The cap is not
   enforced per push — `vm.stack.push` is scattered across the interpreter and
   the builtins, so a per-push bound is invasive — but at *activation*
   granularity: at each call entry, the current depth plus the frame's maximum
   value-stack use must fit under the cap, else the entry throws the same
   `RangeError` the native-stack guard does. That is a handful of check sites
   (the interpreter call path, `run_jit_body`, `run_jit_leaf`,
   `run_compiled_body`), and it keeps the entry ABI unchanged. It needs each
   body's maximum value-stack use known to the interpreter too, which today
   only `max_stack_usage` computes, for the JIT.
2. **Base as an index.** Keep `vm.stack` a growing `Vec` and re-derive the
   working base from `(vm.stack.as_ptr(), work_base)` at each use — the shape
   `frame_get` already uses for the leaf frame. Robust, but every stack access
   pays a reload, so it is a codegen-wide change.

**Carried requirement (2026-10-04).** Mechanism 1 is necessary but not
sufficient. The call helpers push onto `vm.stack` for the duration of the call
and then truncate: `call_slow` (`crates/runtime/src/jit.rs:1863`) and
`call_apply` by `argc + 2`, and `call_vector` (`jit.rs:3654`) by
`vm.stack.extend(args)` where `args` is the spread vector — unbounded
(`f(...bigArray)`), so no compile-time reserve can cover it. A baked
`work_ptr` therefore dangles on the realloc however generous the reserve. The
region needs both halves: the cap/reserve (which bounds the non-call helper
traffic and the `argc`-bounded pushes) **and** a re-derive of the working base
at the call-helper boundaries. Concretely, `JitCallContext` would carry
`work_base` as an index and the compiled code reload `work_ptr =
vm.stack.as_ptr() + work_base * 8` (and `buf_end`) after
`call_slow`/`call_apply`/`call_vector` return.

**Audit correction (2026-10-04): the reload is not a small patch.** The
compiler's stack model is absolute-pointer based. `Lowerer::sp_var`
(`crates/jit/src/compiler.rs:494`) is the working top, `entry_sp_var`
(`:516`) is the run's base (used to reset a self-tail-call back edge), and the
per-handler `handler_sp_vars` (`:497`) are sp snapshots for error resume — all
absolute addresses. One realloc at a call boundary therefore dangles *every*
held pointer, not just `sp_var`, and a `try` can span a call, so the handler
snapshots cannot be rebased by a fixed few instructions. The clean fix is to
make the compiled stack model **base+offset** throughout (the shape mechanism 2
describes): carry a stable base and treat `sp_var`/`entry_sp_var`/
`handler_sp_vars` as offsets, so a moved allocation rebases with one add. That
is a codegen-wide change, not a localized reload — C0a's real cost is that
refactor, and the decision is whether to commit to it.

**Status (2026-10-04): C0a-ii landed via mechanism 1 (non-moving
region).** The first attempt made the region a `vm.stack` segment and re-based
the compiled code after every push-capable helper. Single-threaded it was
green (the full workspace suite passed) but under the parallel harness it
reproduced a layout-dependent write into the freed old allocation
(`STATUS_HEAP_CORRUPTION`/`STATUS_ACCESS_VIOLATION`); it was reverted, and two
findings were recorded for the re-attempt:

1. **A shared movement-delta cursor is consumed by a nested re-base.** Deriving
the new base from a `last_alloc_ptr` in the ctx fails because an in-frame leaf
runs on the *caller's* ctx: the leaf's own re-base advances the cursor, so the
caller's subsequent re-base sees delta 0 and never corrects its (now stale)
base. A per-body slot index (`vm.stack.as_ptr() + base_slot * 8`, preserved by
a realloc) fixes that, but
2. **even the slot-index re-base was not sufficient** — a rare move-triggered
dangling write remained (it vanished when a `reserve` stopped the region
moving). So re-basing sites is the wrong axis.

The re-try removes the move instead of handling it. `run_jit_body`/
`run_jit_resume` place the region on `vm.stack` and `reserve` it to
`stack_cap`, so no helper push during the run can reallocate it — the baked
region pointer stays valid and **no re-base machinery exists at all**. The
`stack_cap` ceiling (C0a-i) is exactly the size the fixed, non-reallocating
stack is sized to. `with_jit_run` drops its buffer argument (the region is
traced with the Vm); `call_slow`/`call_apply` push straight from the region
again (a realloc can no longer dangle it). Residual bound: a same-`Vm` push
beyond the 1M-slot cap would still realloc (the activation guard bounds
regions, not transients) — pathological, and not observed. Verified:
`cargo clippy --workspace --all-targets -- -D warnings` clean; full workspace
suite green; `jit` lib 260/260 across 15 consecutive parallel runs (where the
re-base version aborted); full test262 sweep at baseline (48622 total, 48464
pass, 0 fail/crash/hang, 158 skip).

**Follow-ups (2026-10-04).** The region is released on every run exit
(`vm.stack.truncate(work_base)` in `run_jit_body`/`run_jit_resume`): a
tail-replacement loop recalls `run_jit_body` on the same `Vm`, so a region left
in `vm.stack` would accumulate across iterations. `MAX_VALUE_STACK` is lowered
from 1M to 64K slots, so the per-`Vm` reservation drops from ~8 MB to ~512 KB
— still far beyond any body's operand depth. Stress gates: `jit` lib 260/260
across 10 consecutive parallel runs; full workspace green; normal sweep at
baseline; `--gc-verify` sweep 0 fail/0 crash (its 3 `copyWithin/*detached*
hangs are borderline-slow — 8s normal, 14s under verify — and PASS when re-run
individually); the `--gc-stress` sweep's 59 failures are ALL
`Promise/allSettled` and reproduce byte-for-byte under `--jitless`, i.e. a
pre-existing gc×async interaction, not the region; the CLI `--gc-stress` and
`--nursery-stress` benchmark runs are 12/12 `ok=true`.

Sub-steps, each independently landable and gated on corpus equivalence plus
`--gc-stress`/`--nursery-stress`/the six sweeps with no row regress:

- **C0a — the working region moves onto `vm.stack` at the top level.**
  `run_jit_body` reserves the body's `stack_usage + slack` above the caller's
  `sp` and hands the entry that base (per the mechanism above); the compiled
  frame becomes a `vm.stack` segment, so `frame_get`'s `Frame::Inline/Heap`
  arm serves only the interpreter's own active body. Behavior-neutral.
- **C0b — the leaf lane unifies (landed 2026-10-04).** `run_jit_leaf` runs
  the leaf's frame/working area as a `vm.stack` segment and sets
  `leaf_frame_base` for the run, so a helper's `frame_get` reads the leaf's own
  frame and the working base and `leaf_frame_base` are one offset.
- **C0c — the buffer and its root retire (landed 2026-10-04).** The self-call
  and certified-callee lanes carve their nested frame/working region from
  `vm.stack` too, so `INLINE_JIT_BUF`, the private per-call buffers,
  `Vm::jit_roots`, and `ActiveRun::jit_buffer` all retire: every region is
  `vm.stack`, traced with the Vm.
- **C0d — suspension/resume (landed 2026-10-04).** A suspended body's live
  working region stays on `vm.stack`, normalized to the stack bottom (the
  DEOPT shape) on `DISPATCH_SUSPEND`; `run_jit_resume` re-derives the base as
  the bottom and the live depth as `vm.stack.len()` and extends the region for
  the resume value. `Vm::jit_work` and its trace plumbing retire.

C0 lands nothing measurable by itself; it removes the buffer so C1 can push
nested frames on the same region. Probe: the corpus at parity, and the
private-buffer allocation count under a recursion workload (should be zero).
All four sub-steps landed 2026-10-04: no private JIT frame/working buffer
remains at any lane (body, leaf, self-call, certified-callee, or suspension).

## 5. Traps

- **Per-frame state is the whole difficulty.** `Vm` carries ~15 per-activation
  stacks; a wrong watermark silently corrupts a nested frame (a `try` in a
  callee leaving a handler in the caller). C0/C1 must move a stage at a time,
  each with a fixture that would catch a leaked watermark (nested try/finally,
  for-of, destructure, generator).
- **GC roots.** Nested frames on `vm.stack` are traced; anything the JIT
  keeps in registers across a frame push (SM's "locally traced values") must
  be spilled or traced. `--gc-stress`/`--nursery-stress` gate every stage.
- **Deopt/resume with nested frames.** `vm.ip` and the scratch are per-frame;
  a deopt in a callee must resume that callee, and the caller's frame must be
  intact. The frame descriptor's `saved_*` fields are the resume state.
- **Stack overflow.** One shared stack means a deep recursion of *any* mix of
  interpreted/compiled frames must hit the same `crate::stack::enter_js`
  guard, and the guard must account for frame bytes, not `Vm`s.
- **`Vm` pooling semantics.** Once a call no longer takes a `Vm`, the pool is
  only for *roots*; `return_vm`'s GC reset (`Vm::reset`) no longer runs per
  call — the per-frame watermarks must do that work, or stale roots survive.
- **Tier boundary identity.** A call must resolve to leaf / non-leaf /
  interpreted / builtin with the existing cells; the caches that key on the
  body must not assume one `Vm` per activation.

## 6. Definition of done

- A certified non-leaf call and a `new C()` run as a frame push on the
  caller's `Vm`, with no `take_vm` and no `JitCallContext` rebuild on the
  path (probe: the `non-leaf call` and bare-construct rows).
- `recursive_fib` and the non-leaf call rows fall toward the leaf-call floor
  (~6–7 ns), with the jit-loss gate green and test262 (language + built-ins)
  and the wasm suites at baseline.
- The interpreter and the JIT share the frame: a JIT body can call an
  interpreted body and back without a `Vm` swap.
- A written statement of what remains between this model and V8's/SM's
  (OSR entry, inlined-frame materialization, a mid-tier).

## 7. Open decisions

1. **Frame header in `vm.stack` vs a parallel frame array.** SM/V8 use the
   native stack; we have `Vec<Value>`. Carving from `vm.stack` (recommended —
   `DISPATCH_DEOPT` and the leaf lane already assume it) vs a separate
   `Vec<Frame>`. The former makes GC tracing free.
2. **Whether the interpreter's `Vm` becomes the frame or the frame becomes
   the `Vm`.** C0 picks one direction; the other implies touching every
   `Vm`-reading helper.
3. **Stage C2 in the same change as C1 or after.** C2 is the same protocol
   plus receiver creation; separate landings keep the gates small.
4. **Whether to add a baseline tier** (Sparkplug/SM-Baseline analog) once the
   frame is shared — deferred until C1–C3 land, and only on a measured
   compile-time or coverage constraint (the general-path compile was probed
   and de-scoped once already, `perf.md:120`).
