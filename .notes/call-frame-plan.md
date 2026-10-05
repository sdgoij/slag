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

**C2b — the construct-site verdict cache (emit-side, if the probe warrants).** A
`Construct` site emits one `Helper::Construct` FFI with no site record; if the
C2a probe still shows the per-construct re-derivation (the `leaf_lookup` HashMap
+ verdict for a leaf, the `ecma_functions` lookup for a non-leaf) as the
residual, add a construct-site record (a `ConstructInlineInfo` the site caches,
keyed by the callee's `id` + site, with the same identity/`code_gen`/`epoch`
gate the leaf record uses) and a `Helper::CertifiedConstruct` that skips it — the
construct mirror of C1a/C1b. Probe first.

Probes: a non-leaf certified base constructor row and the bare-construct probe
(`--jit-bench`/the corpus), A/B; `construct_churn` and `recursive_fib` must not
regress. Slices land in order C2a (the measured win), then C2b only if the
residual is still the re-derivation.

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
benches 12/12 `ok=true`. **C2b** (the construct-site verdict cache) remains,
if the probe still shows the re-derivation as the residual.

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
