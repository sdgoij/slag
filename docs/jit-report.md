# Slag JIT — Implementation Report

Status snapshot as of the per-site compiled-body fix (`c2dbcde`).

## 1. Overview & architecture

Slag's interpreter already compiles every function/script body to a linear
`Vec<Step>` bytecode (`CompiledBody`), with **certification** (`ScopeInfo`)
gating the fast paths. The JIT (`crates/jit`) lowers that certified bytecode
to native code via **Cranelift**: `Step` is a stack machine, so each step maps
to a small CLIF sequence, and the certified fast loops
(`FastLoopHead`/`RunRegBody` on the accumulator counter) lower to **real
branch instructions with the loop counter in a register**.

**ABI** — a compiled body is
`extern "C" fn(frame: *mut Value, stack: *mut Value, ctx: *mut c_void) -> u64`:

- `frame` — the body's frame slots, set up exactly like `Vm::setup_frame`
  (params, `var`s, TDZ slots, this slot).
- `stack` — the value-stack base (one-past-the-frame); the JIT pushes/pops
  above it, mirroring the interpreter's `Vec<Value>`.
- `ctx` — the `JitCallContext` (repr(C)): `pending`/`error` (the error ABI),
  `agent`, `vm`, the live `global_object`, the direct-mapped
  `global_value_cells`/`member_value_cells` bases, the `clean_chain` gate,
  the working-buffer end, and the leaf-call cache (`leaf_epoch` +
  `leaf_call_cache`).

**Integration** is one-way (`jit` → `runtime`): `install()` fills
`Agent::jit_hook` with a `JitCache`; the runtime consults it through
`lookup_info`, which reads the per-body `jit_info` `Cell` fast pointer — `0` =
not looked up, `1` = known non-compilable, else a pointer into the cache. Two
run paths:

- **`Vm::run_jit_leaf`** — leaf calls run in-frame on the caller's Vm (private
  frame/working buffer), landing the result like the interpreter's leaf-inline
  run.
- **`run_jit_body`** — general certified bodies (may contain calls) on their
  own pooled Vm.

## 2. What's implemented (commit history)

| # | Commit | Capability |
|---|---|---|
| 1 | `9bb3625` | **Scaffold**: stack ops (`Push`/`Pop`/`Dup`), frame slots (`LoadLocal`/`StoreLocal`/`InitLocal`/`UpdateLocal` + TDZ), arithmetic (number fast path inline, `binary_slow` otherwise), control flow (`Jump`/`JumpIf*`/relational imm tests), the fused canonical loop, member access through helpers, `CallFast`, `Return` + completion steps. W^X `ExecutableCode`. |
| 2 | `c60c870` | Wire into the **Vm leaf-call path** (`run_jit_leaf`). |
| 3 | `f03df37` | CLI `--jit` flag + `--jit-bench` interpreter-vs-JIT benchmark. |
| 4 | `15ce900` | **Compile bodies containing calls** (general path `run_jit_body`, `call_slow`). |
| 5 | `b71c9c2` | **Global and outer-binding access**: `LoadIdent`, `ResolveVarIdent`/`PutVarReference`, `UpdateIdent`, script-level `LoadGlobal`/`StoreGlobal`/`FusedStoreGlobal`. |
| 6 | `ed197a2` | **Compound member assign** (`o.x += 1`). |
| 7 | `9004032` | **Captured bindings via the env machinery**: `LoadContextSlot`/`StoreContextSlot`/`InitContextSlot`/`UpdateContextSlot`, per-iteration forms, and the register (`LeafOp`) forms. |
| 8 | `e7cb203` | Sweep `--jit` flag (test262 conformance through the JIT). |
| 9 | `e894493` | Remaining leaf entry points. |
| 10 | `8504757` | **Hardening**: W^X code pages, bounded cache eviction with in-flight tracking, recursion guard. |
| 11 | `ec1b18b` | E2E tests: fused call-store, TCO tail call, construct leaf. |
| 12 | `246feb6` | **Inline the global fast-cell read** — `LoadGlobal`/`StoreGlobal`/`FusedStoreGlobal` validate a direct-mapped `GlobalValueCell` (name + live global id/generation) in machine code, falling back to helpers on a miss. |
| 13 | `da78be0` | **Inline interpreter fast cells** (member value cells into the member probe). |
| 14 | `e8f7bb8` | **Member stores + fast-loop member reads**: `set_member_slot` in-place property-vector writes, register-path `GetMemberName` probe. |
| 15 | `25e9071` | **Fast string concat** (`concat_strings` helper — string-string `Add` with both tags checked inline) + in-place rope-node construction. |
| 16 | `c2dbcde` | **Fix: share one compiled body per function site** — closes the per-closure recompile trap (function declarations *and* arrows in loops). |
| 17 | *(worktree)* | **Closure creation + trivial context steps** — `CreateFunction`/`CreateArrow`/`FunctionDeclInit`, `NewTarget`, `RegExpLiteral` lower to step-index helpers that read the step's payload back out of the running body (`JitCallContext::body`) and run the interpreter's instantiation/evaluation machinery against the live lexical environment. A loop creating closures now runs entirely in machine code; the created closure's own body compiles separately. |

### Slow-path helper table (`JitSlowPaths`, 38 helpers)

The JIT inlines the number/string fast paths (tag checks are ~2 instructions
on NaN-boxed values); everything else calls a helper whose address is baked
into the machine code at compile time. The table: `binary_slow`,
`concat_strings`, `relational_slow`, `update_value_slow`, `to_boolean_slow`,
`tdz_error`, `get_member_name`/`get_member_computed`,
`set_member_name`/`set_member_computed`, `set_member_slot`,
`assign_member_name`/`assign_member_computed`, `call_slow`,
`leaf_call_probe`, `get_global`/`set_global`/`set_global_slot`, `load_ident`,
`resolve_var_ident`/`put_var_reference`/`put_var_reference_op`/
`get_var_reference`/`update_var_reference`/`pop_var_reference`,
`update_ident`, `load_context`/`store_context`/`init_context`/`update_context`,
`load_per_iter`/`store_per_iter`/`update_per_iter`,
`create_function`/`create_arrow`/`create_function_decl`/`new_target`/
`regexp_literal` (the step-index helpers). A body needing a `None` helper
bails to the interpreter.

## 3. Fast-path machinery

- **Global value cells** — 256 direct-mapped cells keyed by
  `name & (GLOBAL_CELLS-1)`, each holding `name`, `global_id`, `generation`,
  `slot`, `value`. The compiled `LoadGlobal` reads the cell in place and
  validates against the **live** global object's id/generation (re-read from
  the object each iteration), so a mid-run mutation (via helper, getter,
  `defineProperty`) invalidates the cell and falls to `get_global`.
  `StoreGlobal` similarly writes the property vector via `set_global_slot` on
  a validated shape.
- **Member value cells** — the compiled `GetMemberName` probe indexes by
  `(object_id ^ name) & (MEMBER_CELLS-1)` and validates id/name/live-
  generation before loading the value; `set_member_slot` writes in place with
  the same validation.
- **Leaf-call-site cache** — each compiled `CallFast*` site caches the
  `leaf_call_probe` verdict (step index + `leaf_epoch` + full callee NaN-box
  identity); a matching record skips the probe entirely. `leaf_epoch` is
  bumped after any "disturbing" helper (getter/setter/`valueOf`/nested call)
  that could re-enter the interpreter and change eligibility.
- **`clean_chain` gate** — the compiled `LoadIdent` probe is only sound when
  the body's env chain is *exactly* the global env; nested bodies (named-
  function-expression scopes, blocks, `with`, modules) fall back to the
  `load_ident` resolve.

## 4. Safety / hardening

- **W^X**: `ExecutableCode` allocates RW, copies the code, then
  `region::protect` → RX. Test: `allocation_is_read_execute_after_the_copy`.
- **Bounded cache**: LRU clock; `MAX_CACHE_ENTRIES = 256`, evicts to
  `EVICT_TO_ENTRIES = 128`; an evicted entry clears the body's `jit_info`
  fast pointer so the next call recompiles; **eviction is suppressed while a
  compiled frame is executing** (`in_flight`) so a live entry pointer on the
  native stack can't be freed.
- **Recursion guard**: `Agent::jit_depth` with `MAX_JIT_DEPTH = 128`; both
  run paths fall back to the interpreter at the cap so private buffers can't
  exhaust the native stack.
- **GC rooting**: `with_jit_run` registers the Vm + working buffer for the
  run's duration; `run_jit_leaf` pushes `jit_roots`; the value cells, leaf
  caches, and the `compiled_bodies` site cache are all traced. `--gc-stress`
  e2e tests cover both the leaf buffer and the general frame.
- **Error ABI**: a throwing helper sets the context's `pending` byte (offset
  0); the compiled body stops immediately and the runtime surfaces the error.

## 5. Validation

**Benchmarks** (`--jit-bench`, release):

| row | before fast-cell work | now | ratio |
|---|---|---|---|
| arithmetic | 15.1ms | 3.1ms | **0.23** |
| property read | 30.5ms | 6.4ms | **0.23** (was 0.62) |
| string concat | 10.1ms | 2.7ms | **0.27** (was 0.44) |
| function calls | 9.2ms | 2.3ms | **0.25** (was 0.56) |
| global read | 155.7ms | 6.2ms | **0.04** (was 0.63) |
| compound assign | 21.7ms | 3.1ms | **0.13** (was 0.77) |

**Conformance** (full-area `--jit` sweeps): `language` **23721 pass / 0 fail /
0 crash / 0 hang** (3 skip); `annexB` 1086 pass / 0 hang; `built-ins` 23212
pass / 445 hang — *pre-existing* (baseline 447) slow RegExp property-escape
fixtures, unrelated to the JIT. Clusters: `expressions/call` 92/92,
`arrow-function` 343/343.

**Tests**: `cargo test --workspace` green (4426; jit crate 61 incl. the
`installed_jit_*` e2e tests for member/slot callees, loop-with-calls, mid-run
cell mutation, GC-stress rooting, global store-then-read, scope-shadow
correctness); `cargo clippy --workspace --all-targets -- -D warnings` clean.

**The headline fix**: `tco-call-args.js` (a `getF()` closure per TCO step)
went **15.9s → 0.26s** under `--jit`. Root cause:
`register_function`/`instantiate_arrow` recompiled the body IR per closure
instantiation, and the per-body `jit_info` pointer made the JIT recompile
Cranelift machine code per closure (~147µs × 100K). The fix shares one
`Rc<CompiledBody>` per declaration site (`Agent::compiled_bodies`, keyed on
the shared body `Rc<Block>`; arrows needed a stable `&ArrowBody` node identity
first via `shared_arrow_body`).

## 6. What still bails (runs on the interpreter)

From the crate docs and the compiler's `Unsupported` surface — bodies
containing any of these fall back entirely:

- **Closure creation** (`CreateFunction`, `CreateArrow`, `FunctionDeclInit`),
  `NewTarget`, and `RegExpLiteral` are now **compiled** (Cut 44) — see the
  implemented table.
- **Env machinery**: `EnterBlock`/`LeaveBlock`, `EnterWith`, `EnterTry`/
  `Exit`/`CatchBind`/`FinallyEnd`, `PerIteration`/`EnterLoopEnv`/
  `EnterPerIteration` (creation), `UsingInit`/`DeclInit`.
- **Iterator machinery**: all `ForIn*`, `ForOf*`, `AsyncForOf*`.
- **Suspension**: `Yield`/`Await`/`YieldStar*`/`AsyncYieldStar*`
  (generators/async).
- **Control machinery**: `SwitchDisc`/`SwitchTest`, class steps (`ClassBegin`/
  `ClassHeritage`/`ClassFinish`…), `RegExpLiteral`.
- **Destructuring/spread**: `ObjectSpread`, `ArraySpread`, all `Destructure*`.
- **Sloppy mapped arguments**: `CreateArguments { mapped: Some }`.
- **Super machinery**: `SuperCall`, `GetSuperBase`, `GetSuperName`/
  `GetSuperComputed`/`AssignSuper*`, `DeleteSuper`, `UpdateSuper*`.
- **Misc**: `ThisValue`/`NewTarget` (leaf exclusion), direct eval in a
  `CallFast` site, heap-value constants (only non-heap constants inline).

## 7. TODO / next steps

**Correctness/soundness gaps (same shape as the fixed bug):**

1. **`instantiate_accessor`** still builds a fresh `Rc<Block>` per
   instantiation — an accessor created inside a loop would recompile IR + JIT
   code per iteration, exactly like the function/arrow trap. Fix shape is
   identical: share the block per site. (Accessors-in-loops are rare, hence
   deferred.)
2. **Script/eval bodies** are not in the per-site compiled-body cache
   (`compile_statements`), so re-evaluating the same source recompiles. Lower
   priority (scripts aren't leaf callees, and repeat-eval-in-loop is uncommon).
3. **`params.clone()` per closure instantiation** (both `register_function`
   callers and arrows) — minor per-instantiation allocation; could share the
   params `Vec` per site too.

**Performance (the remaining benchmark rows and hot shapes):**

4. **Inline direct leaf calls further** — `function calls` is 0.25 and the
   leaf probe already caches verdicts, but a statically-known leaf callee at a
   stable slot could skip the probe and `call_slow` round-trip entirely.
5. **Closure-creation lowering** (`Step::Closure`/`CreateFunction`/
   `CreateArrow`) — **done** (Cut 44): the creation step now compiles via
   step-index helpers; the remaining cost is the instantiation machinery
   itself (env + object allocation), which a future inline fast path for
   capture-free closures could cut.
6. **Extend the bail list**: `try/catch/finally`, `switch`, `with`, `using`,
   iterators, generators/async, destructuring/spread, class machinery, mapped
   `arguments`, tail calls (`TailCall`/`TailCallFast` — a TCO-recursive body
   still bails at the recursion) — each is a slice of lowering + helper
   work.
7. **String concat**: the `concat_strings` helper is called per concat; very
   small strings could concat inline in registers.
8. **Leaf-cache invalidation breadth**: any "disturbing" helper (a `valueOf`,
   a getter) bumps `leaf_epoch` and drops the per-site leaf verdict — a
   monomorphic hot call next to a `valueOf` re-probes every iteration.

**Non-JIT engine work observed along the way:**

9. **`built-ins` RegExp property-escape fixtures** (~445) are the slowest
   cluster in the sweep — pre-existing RegExp compilation cost, independent of
   the JIT.
10. **`--gc-stress` is superlinear** on 100K-closure fixtures (per-allocation
    collection × per-iteration allocations) — inherent to the mode, not a
    regression.
