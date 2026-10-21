# Rust-initiated calls: closing the 15x gap to a compiled call site

**Status: PLAN.** Measured 2026-10-21. Nothing here is implemented. The numbers
live in `.notes/perf.md` ("own accessors skip the chain walk…").

## 1. The shape of the gap

A function body called from *compiled* code and the same body called from
*Rust* (an accessor, a builtin callback) differ by more than an order of
magnitude:

| probe (1M iterations) | ns per call |
| --- | ---: |
| `o.m()` — method call from compiled code, body `return 1` | **9.2** |
| `o.m()` — method call, body `return this.a + 1` | 10.8 |
| `o.x` — **getter**, body `return 1` | **156** |
| `o.x` — getter, body `return this.a + 1` | 154 |
| `g.call(o)` — `Function.prototype.call` from compiled code, same body | 27.5 |
| `o.nope` — property that does not exist (no call at all) | 105 |
| own data property `o.a` (control) | 5.6 |

The getter's body is irrelevant: a constant and a member read cost the same.
So the 145 ns is the *invocation path*.

## 2. What actually runs

Correction to an earlier note: `ordinary_call` does **not** bypass the JIT. It
holds no `agent.jit_hook` reference, but it calls `run_compiled_body`, which
calls `crate::jit::run_jit_body`. Machine code is entered. The cost is the
scaffolding around it.

**Compiled call site** (`o.m()`): the emitted `call_slow`/leaf probe reuses the
**caller's live `JitCallContext`** and carves the callee's frame out of the
caller's own working buffer (`try_shared_construct_leaf`'s model, applied to
ordinary calls by `run_jit_leaf`). Nothing is built per call.

**Rust-initiated call** (`crate::function::call` → `call_inner` →
`ordinary_call` → `run_compiled_body`), per call:

That this is avoidable is shown by `Function.prototype.call`: `g.call(o)` runs
from the same compiled loop, is dispatched through `do_call_apply` →
`do_call_fast` with the caller's live context, and costs **27.5 ns** for a body
that the accessor route takes 154 ns to run. The difference is the lane, not
the callee.

1. `agent.take_vm(env, strict)` — pop a pooled `Box<Vm>`, `rebind`.
2. `push ExecutionContext` (a struct with two `EnvRef` clones).
3. `scope.new_body_context` / capture-context setup.
4. `OrdinaryCallBindThis` (`this_slot` present) — sloppy coercion can box.
5. `vm.setup_certified_frame(scope, args, this_value)` — slot-by-slot fill.
6. `crate::jit::run_jit_body` — depth guard, `lookup_info` (a HashMap on the
   body pointer), a **~40-field `JitCallContext` literal**, the entry call,
   then outcome dispatch.
7. `agent.return_vm(vm)` — `vm.reset(...)`, which clears the working stack and
   resets every run field (deliberately: GC-4 requires the pooled Vm not to
   trace a finished run's state).
8. `dispose_env_resources`.

`call_callback` (the HOF path) is not better: it tries
`builtins::function::try_leaf_call`, which does its own `take_vm` and
`do_call_fast`. Both Rust paths pay steps 1 and 7.

## 3. Who pays it

- **Accessor getters/setters** — `get_property`/`put_value` →
  `crate::function::call`. Every `re.source`, every typed-array `length`, every
  user getter.
- **Builtin callbacks** — `forEach`/`map`/`filter`/`some`/`every`/`reduce`,
  `sort` comparators, `replace` callbacks, promise reactions
  (`call_callback` → `try_leaf_call`).
- Any host or builtin that calls back into JS.

The compiled lane is not available to any of them, because none of them runs
inside a `JitCallContext`.

## 4. Options

### A. Reuse the `JitCallContext` instead of rebuilding it (small, low risk)

`run_jit_body` builds ~40 fields per call. Keep one `JitCallContext` on the
`Vm` and re-point only the per-run fields (agent/vm pointers, global, the cell
array bases, `buf_end`, `gc_ticks`, `intrinsic_bits` when the body has an
intrinsic site). Saves maybe 20-40 ns of the 154.

Cheap to land and independently valuable, but it cannot get near 9 ns.

### B. Publish the live context so a Rust call can use the shared lane (the real fix)

When the accessor is invoked **from compiled code**, a `JitCallContext` is
already live — the helper that reached `get_property` was called by machine
code with it. So the machinery to run the getter cheaply is already in scope;
nothing publishes it.

Sketch:

- Add `Agent::live_ctx: Option<(*mut JitCallContext, sp, buf_end)>`, set by the
  JIT helpers that can re-enter (`get_member_name`, `get_member_computed`,
  `set_member_name`, …) for the duration of the call, restored after.
- `function::call` (and `call_callback`) first try **the shared lane**: the
  callee is leaf-eligible (`steps_are_leaf`), environment-free (or the
  `LeafCallEnv` env-swap variant), its frame fits above `sp`, and
  `agent.jit_depth` is under the cap. Then run its compiled entry on the live
  ctx with the frame carved from the caller's buffer — exactly
  `try_shared_construct_leaf`, generalized from construct to call.
- Fall back to today's path for a non-leaf callee, a non-certified body, an
  environment-reading leaf, no room, or no live ctx.

Expected: a leaf getter goes to the ~10-20 ns lane. A non-leaf getter keeps
today's cost, which is the same call it would make from machine code anyway.

### C. Accessor-only variant (narrower)

Skip the `Agent::live_ctx` field: pass the ctx down through
`function::call` from the one caller that has it (`get_member_name`). Same
mechanics, smaller blast radius, but does not help `call_callback`, which has
no ctx either (a `forEach` running in Rust has no caller ctx).

## 5. Recommended first slice

1. **A**, measured alone: if the accessor read does not move below ~120 ns, the
   ctx build is not the dominant term and B/C need remeasuring first.
2. **C** (accessor only) before B: it is the smallest change that reaches the
   shared lane and the easiest to validate — a getter is a single call, no
   argument list marshalling.
3. **B** only once C is validated.

## 6. Traps

- **The callee may be non-leaf.** Anything with a call step disqualifies a
  leaf (`steps_are_leaf`), including `CallApply`. Falling back is required, and
  the fallback must be the *entire* old path, not a partially built Vm.
- **Environment-reading leaves need the env swap** (`LeafCallEnv`), which is
  the whole reason `run_jit_leaf` exists. A getter that captures a variable is
  common (`get x() { return this.a + k; }`), so the env-swap variant is not an
  edge case.
- **Room above `sp`.** The shared lane carves the callee frame from the
  caller's working buffer; `try_shared_construct_leaf` refuses when it does not
  fit and the caller falls back. The same guard is mandatory here, and a
  too-small buffer must be a fallback, never a write past `buf_end`.
- **GC rooting.** The caller's whole buffer is rooted for its run (that is what
  makes the shared lane safe). A Rust-side private buffer is not — which is
  exactly why `try_leaf_call` registers `with_leaf_run`. The shared lane
  inherits the caller's rooting, so it must not allocate into anywhere else.
- **Re-entrancy.** A getter that calls back into JS needs its own context; the
  live-ctx pointer must be saved/restored, not just set, and `jit_depth` still
  applies.
- **Sloppy `this` binding.** `OrdinaryCallBindThis` boxes primitives and maps
  nullish to the global object; the shared lane must apply the same rule (the
  construct lane already does for its `this` slot).
- **A stale `live_ctx`.** Any helper that returns to machine code must clear
  it; a helper that panics must not leave it dangling.
- **The accessor path is the outlier, not calls in general.** `g.call(o)` on
  the same body is 27.5 ns, so a Rust-reached call is not inherently slow; the
  accessor's extra ~125 ns is its own path.
- **Probe hygiene.** An earlier `g.call(o)` probe (getter created inside the
  hot function's object literal and recovered through
  `Object.getOwnPropertyDescriptor`) measured 450 ns. The clean shape is 27.5.
  Define throwaway functions outside the measured body.

## 7. Validation

Per slice: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets
-- -D warnings`; `cargo test --workspace`; four sweeps at `--timeout 15`
(51,998 fixtures, 0 fail); the probe set in `scratch/ab/re2` (accessor, data,
missing, `re.test`) as medians of ≥7 interleaved same-binary pairs; and the
corpus differential — remembering that this host moves ±10% and that the family
table is the stronger signal for a cross-cutting change.
