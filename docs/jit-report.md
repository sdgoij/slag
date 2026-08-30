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
| 18 | *(worktree)* | **Proper tail calls** — `TailCallFast`/`TailCallFastGlobal`/`TailCallFastSlot` (strict-mode TCO; the sloppy form stays a normal call per spec) lower to a `tail_call` helper that mirrors `tail_call_shared`: an ordinary certified callee replaces the current frame on the Vm (`JitCallContext::tail` + `Vm::tail_replaced`), anything else is a normal call whose result completes the body's return. `run_compiled_body` loops on the replaced body with the same Vm, so a 100K-deep TCO chain never grows the native stack. |
| 19 | *(worktree)* | **Self-tail-call as a jump** — `TailCallSelf` (Cut 46): a tail call whose callee is the enclosing named function expression's own immutable self-binding compiles to an in-place frame rebind + jump back to the body's re-entry block, so the whole self-recursive tail chain runs in ONE machine-code invocation (no `tail_call` helper, no `run_compiled_body` round-trip). The compiler emits it only for certified capture-free, arguments-free bodies with the self-name resolving to the `Env` walk; the interpreter's `tail_call_self` rebinds the frame and re-enters the dispatch loop for the same shape. Measured ~3.3× faster than the round-trip path on a 1M-iteration chain (37ms vs ~120ms). |
| 20 | *(worktree)* | **Global-name self-tail-call check** — `TailCallSelfCheck` (Cut 47): a tail call to the enclosing function's own NAME in a body that is not a named expression (`function f(n) { return f(n - 1); }`) — the name resolves through the global/outer env and could have been reassigned, so the machine code compares the resolved callee against the running closure (`Vm::current_function` captured into `JitCallContext::current_function`, exact bits) and jumps to the re-entry block on a match, else runs the `tail_call` helper. The interpreter routes it through the shared `tail_call_shared` machinery. The 1M global-name chain drops from ~120ms to ~38ms, matching the static form. |
| 21 | *(worktree)* | **Vector call form** (Cut 49): `ArgsBase`/`ArgsPush`/`ArgsSpread` and the vector `Call`/`TailCall` steps — a ≥3-argument or spread call (which previously bailed the whole body to the interpreter) — lower to helpers that build the argument vector in `Vm::args` and run the interpreter's vector handlers (`do_call`/`tail_call`), bridging the work-buffer operands like `call_slow`. A 3-arg loop compiles for the first time; a ≥3-arg TCO chain runs with bounded stack. |
| 22 | *(worktree)* | **Fast-argument cap raised to 8 + direct-eval vector routing** (Cut 50): `FAST_CALL_MAX_ARGS = 8` — plain calls with 3–8 args now take the fast `CallFast`/`TailCallFast` form (leaf-inline eligible) instead of the vector form; a direct `eval` ALWAYS takes the vector form (the fast form's JIT path bails on eval, and the vector handlers route it correctly). `call_vector` routes through `do_call_fast` (the same leaf-inline core as a `CallFast` site); `run_inline_leaf`'s argument buffer grew to `FAST_CALL_MAX_ARGS` with a Vec fallback for the (rare) vector-form count beyond it. A 1M 3-arg leaf call drops ~220ms → ~95ms under `--jit`. |
| 23 | *(worktree)* | **Vector-form self-tail-call as a jump** (Cut 51): `TailCallSelfVector`/`TailCallSelfCheckVector` — a self-tail-call whose arguments took the vector form (a spread, or more than `FAST_CALL_MAX_ARGS` plain args) now rebinds the frame in place from the Vm's argument vector (`tail_call_self_vector` helper — no frame realloc, the JIT's frame pointer stays live) and jumps back to the body's re-entry block, the same single-invocation self-chain as the fast-form jump. The checked form compares the resolved callee against `current_function` first and falls back to the general vector `tail_call` helper on a mismatch. A 500K-deep 10-arg self-tail-call runs ~27ms under `--jit` (vs ~62ms interpreter; the pre-jump round-trip was ~10µs/iteration). |
| 24 | *(worktree)* | **Array-literal lowering** (Cut 52): `ArrayBegin`/`ArrayElement`/`ArraySpread`/`ArrayHole`/`ArrayEnd` — the five array-literal steps — lower to helpers mirroring the interpreter's handlers: `array_begin` creates the array and opens an index on the Vm's array-index stack, the element/spread helpers define elements at that index, `array_end` pops it and sets `length`. The array rides the work stack between the steps (a heap object). `run_jit_leaf` now saves/restores the array-index stack (a leaf may build an array on the caller's Vm, and a throw mid-literal must not leak the entry). A spread self-tail-call (`f(...[n - 1])`), previously bailed by the array literal, now compiles and jumps: 200K iterations drop from ~2s (full interpreter bail) to ~430ms under `--jit`. |
| 25 | *(worktree)* | **Dense-array fast spread** (Cut 52b): the spread helpers (`args_spread`, `array_spread`) reuse the for-of machinery's `for_of_begin` verdict — a plain Array with the stock `@@iterator` iterates via the generation-validated element cache (`Vm::array_length`/`array_element_get` with a full-Get fallback, mirroring `for_of_next`) instead of creating the iterator object and calling `next()` per element; the generic path uses `for_of_begin`'s record (the `@@iterator` getter fires exactly once). The spread self-tail-call drops to ~410ms and `[...x]`/`f(...x)` spreads of arrays speed up ~3x; the array-creation machinery (not the iteration) is now the floor. |

### Slow-path helper table (`JitSlowPaths`, 49 helpers)

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
`regexp_literal` (the step-index helpers), `tail_call` (proper tail calls),
`args_base`/`args_push`/`args_spread`/`call_vector`/`tail_call_vector` (the
vector call form, Cut 49), `tail_call_self_vector` (the vector-form
self-tail-call rebind, Cut 51), `array_begin`/`array_element`/`array_spread`/
`array_hole`/`array_end` (the array-literal steps, Cut 52). A body needing a
`None` helper bails to the interpreter.

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
  implemented table. Same for **proper tail calls** (Cut 45, incl. the
  fused global/slot forms and — Cut 49 — the vector form `TailCall`), with
  the NFE (Cut 46), global-name-checked (Cut 47), and vector-form (Cut 51)
  self-tail-calls jumping in machine code; sloppy-mode `return f(n-1)`
  compiles as a normal call (TCO is strict-only by spec).
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

1. ~~**`instantiate_accessor`**~~ — **done** (Cut 48): the accessor body
   `Rc<Block>` is now shared per site (`shared_accessor_body`, mirroring the
   Cut 43 function/arrow cache), so the compiled IR + JIT code compile once
   per getter/setter site instead of per instantiation. Measured on an
   accessor-in-a-loop with a per-iteration call: `--jit` 1100ms → ~82ms
   (~13×) and the interpreter 120ms → ~80ms (the per-instantiation IR
   recompile was hurting both paths).
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
6. **The instantiation machinery is the floor** (measured 2026-08-30): a
   closure-creation loop measures ~7.8µs per closure (interp AND jit — the
   cost is `register_function`'s object setup: `Function::new`, the
   `prototype` object + name/length properties, the `ecma_functions` insert),
   and a plain `{ a: 1 }` object literal ~17µs per literal — the JIT never
   touches these. Cutting it needs a fast path in `register_function`/
   object creation (share the params Vec per site, skip the intermediate
   descriptors, pool the prototype object) — the report frontier.
7. **Extend the bail list**: `try/catch/finally`, `switch`, `with`, `using`,
   iterators, generators/async, destructuring/spread, class machinery, mapped
   `arguments` — each is a slice of lowering + helper work. Proper tail calls
   are **done** (Cut 45); the NAMED (Cut 46), global-name (Cut 47), and
   **vector-form** (Cut 51 — a spread or >8 plain args now rebinds the frame
   from the Vm's argument vector and jumps) self-tail-call forms run in
   machine code; the **vector call form** (Cut 49) is done, and **array
   literals** (Cut 52 — the five array steps lower, with a dense-array fast
   spread in the spread helpers) no longer bail, so a spread self-tail-call
   compiles and jumps end to end (~430ms for 200K, down from the ~2s
   interpreter bail; the array-creation machinery is the remaining floor).
   A vector call to a certified LEAF still runs the general `call_inner` (no
   leaf-inline — a future slice could rebuild the fast-form layout in machine
   code), and a computed callee (`getF()(n-1)`) still pays the per-iteration
   `tail_call` helper round-trip. Object literals (`ObjectBegin`/`Init`/
   `End`) are the analogous next literal shape to lower.
8. **String concat**: the `concat_strings` helper is called per concat; very
   small strings could concat inline in registers.
9. **Leaf-cache invalidation breadth**: any "disturbing" helper (a `valueOf`,
   a getter) bumps `leaf_epoch` and drops the per-site leaf verdict — a
   monomorphic hot call next to a `valueOf` re-probes every iteration.

**Non-JIT engine work observed along the way:**

10. **`built-ins` RegExp property-escape fixtures** (~445) are the slowest
   cluster in the sweep — pre-existing RegExp compilation cost, independent of
   the JIT.
11. **`--gc-stress` is superlinear** on 100K-closure fixtures (per-allocation
    collection × per-iteration allocations) — inherent to the mode, not a
    regression.
