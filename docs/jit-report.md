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
| 26 | *(worktree)* | **Object-literal lowering** (Cut 53): all nine object-literal steps lower — `ObjectBegin` creates the plain object with the realm's `Object.prototype`; `ObjectInitName`/`ObjectInitComputed` define data properties (the `__proto__` setter special case and name inference preserved); `ObjectKeyToPropertyKey` converts a computed key before the value evaluates; `ObjectMethodName`/`ObjectMethodComputed`/`ObjectAccessorName`/`ObjectAccessorComputed` define methods/accessors through step-index helpers (the Cut 44 pattern — the function/param/body payload is read back from the running body, so method instantiation runs the shared `instantiate_method`/`instantiate_accessor` machinery); `ObjectSpread` copies a source's own enumerable properties. A body with an object literal compiles instead of bailing: a `{a: n, b: n+1}` loop runs 76ms vs 87ms interpreter for 200K; computed/spread/method shapes are machinery-bound (ToPropertyKey, copy-data-properties, function instantiation). |
| 27 | *(worktree)* | **String-literal lowering** (Cut 54): a plain string/bigint literal (`Push(Value::String(...))` — `compile_literal` emits a heap `Push`, only templates use `PushStr`) lowers via `push_const`; template quasis (`PushStr`) via `push_str`; the template flatten concat (`ConcatStr`/`ConcatStrConst`) via the `concat_strings`-adjacent helpers; and a register body's heap constant (`LeafOp::LoadConst`/`BinConst`, member `RegOperand::Const`) via `load_const` — a step/op/field-indexed helper reading the value back from the running body. Bodies with string literals — previously bailing wholesale — compile: a string-literal assignment loop runs 7ms vs 15ms interpreter for 2M. The step-index helpers read the RUNNING body's payload, so the steps are excluded from leaf-inlining (an inlined leaf's helpers would see the CALLER's `JitCallContext::body` — the fix that caught a 35-fixture crash in the dynamic-import cluster). |
| 28 | *(worktree)* | **try/catch/finally + control-transfer dispatch** (Cut 55): the biggest remaining bail item. Certification (`FastScopeScan`) now accepts `try` — the try block/finalizer scan as blocks, the catch parameter as a flat frame slot (a simple uncaptured Ident; captured/destructuring params keep the body on the env path), the catch body one depth deeper so a try-block reference to the parameter bails. The steps lower: `EnterBlock`/`LeaveBlock` push/pop real block envs, `EnterTry` pushes the `TryFrame`, `Exit`/`Return`/`Break`/`Continue`/`Throw`/`FinallyEnd` run the interpreter's `control_transfer`/`throw_machinery` through new dispatch helpers that return a step target (the machine code branches over the body's static transfer-target set) or a completion sentinel, and `CatchBind` mirrors the interpreter handler plus writes the flat param slot. A helper's pending engine error in a try body routes through `dispatch_error` (the interpreter's Err arm) instead of terminating. Try/catch envs are marked context-transparent in certified bodies (like per-iteration envs) so closures created inside them still reach their capture context. Bodies with plain blocks, breaks, continues, and throws also compile now. |
| 29 | *(worktree)* | **switch** (Cut 56): certification accepts `switch` — the discriminant and case tests scan at the outer depth (they run before the shared case-block's bindings initialize), the case consequents as one shared block scope. `SwitchDisc` stores the popped discriminant via a helper; `SwitchTest` strictly-equals each case test against it and the machine code jumps to the matched case block (a static target) on a match, falling through to the next test otherwise — no dispatch table, the `break` out of a switch rides the Cut 55 control dispatch. A switch case body with a DIRECT Annex B block function declaration stays on the env path (the compiler's block-scope copy machinery is only activated for `StmtKind::Block`, and the case bodies compile as bare statement lists — 2 annexB fixtures caught this). A hot switch loop compiles; nested switches and switch-in-try shapes work. |
| 30 | *(worktree)* | **for-in / for-of + per-iteration envs** (Cut 57): the certified `ForOf*`/`ForIn*` steps lower — `ForInBegin`/`ForOfBegin` open the enumeration/iteration through the interpreter's shared machinery (`for_in_key_levels` / `for_of_begin`, which keeps the dense-array fast verdict); `ForInNext`/`ForOfNext`/`ForOfNextBindLocal` advance it (the fast path re-reads length + element per step via the generation-validated caches; a generic `next()` error sets `for_of_stepping` so the close machinery skips the iterator, spec 14.7.6.2's `?`); the element lands on the working stack (or the fused frame slot) and the machine code jumps to `back`/`done` with static conditional branches (the Cut 56 switch pattern, no dispatch table); `ForOfClose` pops the boundary + closes a generic iterator; `ForOfBindLocal`/`ForOfBindGlobal` bind the element inline / via the existing `set_global` helper. A captured lexical head emits the per-iteration envs: `EnterPerIteration` (first env, pushed) and `PerIteration` (fresh copy per later iteration, replacing without pushing) via step-index helpers — a `for (let i of a) { ar.push(() => i) }` body compiles. Three correctness closures the cut needed: a for-of body's `Return` routes through `return_control` (its `control_transfer` closes the iterator on the escape); a helper's engine error in a non-try for-of body closes the active iterators before the pending error surfaces (`for_of_close_all`, mirroring `run_inner`'s uncovered-error close but skipping when `for_of_stepping` — a `next()` error); and `throw_machinery`'s escaping-throw close is gated on `!for_of_stepping` (the JIT's pending-error dispatch routes such errors through it). For-in has no close (no iterator); `AsyncForOf*` stays bailed (async bodies never certify). |
| 31 | *(worktree)* | **Generator/async suspension** (Cut 58): a certified generator or async-function body compiles, with `Step::Yield`/`Step::Await` lowering to `yield_suspend`/`await_suspend` — helpers that record the suspension payload (value + delegate flag for `yield`, the awaited value for `await`), the machine code's working-stack pointer (`suspend_sp` — the depth `run_jit_body` saves into the new traced `Vm::jit_work`), and the continuation step (`vm.ip`), then signal `DISPATCH_SUSPEND` (`u64::MAX - 2`). A body with any suspension gets an entry dispatch on `ctx.resume_kind`: 0 (normal resume) compares `ctx.resume_ip` against the static `suspension_targets` and jumps to the continuation with the resume value pushed on the restored region; 1/2 (throw/return resume) route `ctx.resume_value` through `throw_control`/`return_control`. `resume_ip == 0` is the fresh-run sentinel. The drivers (`run_jit_body_loop`/`run_jit_resume_loop`) loop on `TailReplaced`, convert a completed value to the `Completed(Return)` outcome, and fall back to `vm.start`/`vm.run`/`vm.run_abrupt` (the abrupt-of-a-plain-`yield`/`await` decision mirrors `resume_body`). Resumable bodies are never leaves (a no-`await` async body must still route through `call_async_function` to return a Promise; a no-`yield` generator body through `call_generator`); the generator/async drivers install the capture context (`new_body_context`) into `vm.body_context` + `vm.lexical_env` + the saved ExecutionContext's `lexical_environment` so closures created mid-body capture it, and `setup_certified_frame` fills the frame from the saved call args/this. `yield*` and async generators (`AsyncYieldStar*`) stay bailed: certification rejects `delegate: true` and the combined async+generator kind. |
| 32 | *(worktree)* | **Destructuring** (Cut 59): certification accepts destructuring declaration patterns (the scan walks the pattern's bound names into the frame/context slot layout, scanning defaults and computed keys as expressions) and destructuring-assignment targets whose elements are identifiers or nested patterns (member targets stay on the env path — they need the reference machinery). The compiler emits the primitive `Destructure*` steps for EVERY certified pattern — `DestructureBegin`/`DestructureNext`/`DestructureUndef`/`DestructureRest`/`DestructureClose` drive an array pattern's iterator through helpers mirroring the interpreter handlers (the element lands on the working stack; `DestructureUndef` is a static conditional jump to the fixup-patched default label, the Cut 56/57 branch pattern); `DestructureObjCoercible`/`DestructureObjKey`/`DestructureObjKeyComputed`/`DestructureObjKeyStore`/`DestructureObjKeyGet`/`DestructureObjRest`/`DestructureObjEnd` drive an object pattern's key reads, computed-key store/get pair, and rest copy (the static exclusion set read from the step payload). The certified binds are flat: a declaration element writes its slot/context via `InitLocal`/`InitContextSlot`, an assignment element via `StoreLocal`/`StoreContextSlot`/`StoreGlobal` (an undeclared assign target falls back to `AssignIdent` — it bails the body at JIT time, correct but slower). A for-loop head's destructuring (`for (var [...x] = iter; ;)`) compiles the same steps (the var/lexical head paths were the two sweep regressions this cut's certification exposed); a lexical pattern head whose name a body closure captures stays on the env path (per-iteration freshness covers only ident heads). Error handling mirrors `run_inner`'s Err arm: a step error in a destructure body closes the active not-done iterators (`destructure_close_all` on the non-try error path; `dispatch_error` closes before the handler-table routing) unless `destructure_stepping` (a `next()` error leaves the iterator open — the interpreter's exact behavior), and an abrupt `yield`/`await` resume inside a pattern closes them like `run_abrupt_inner`. The wholesale `Destructure`/`DeclInit` pattern binds (env machinery) and `SetFunctionName` (anonymous-function defaults) stay bailed. |
| 33 | *(worktree)* | **Arguments objects** (Cut 60): `Step::CreateArguments` — the last bail item's `mapped: Some` form — lowers via a step-index helper reading the `slot`/`mapped` payload: the sloppy MAPPED object (`create_mapped_arguments_object` — aliasing the simple params through the capture context `vm.lexical_env`, which the mapped-arguments certification slice already moved every param into; `arguments.callee` from the running context's `function`) and the strict UNMAPPED object (`create_unmapped_arguments_object` — reads only the call's argument slice). Both were previously bailed for non-leaf bodies (only the leaf path handled the unmapped form). `Step::TypeofTop` (a `typeof` VALUE operand — `typeof arguments.callee` and any member/computed `typeof`) lowers too: a pure helper returning the `typeof` string, whitelisted as leaf-eligibility-neutral; `TypeofIdent` (the unresolvable-reference form) stays env-path. BOTH `CreateArguments` forms stay leaf-excluded: the helper writes the body's `arguments` slot through `vm.frame`, but a JIT leaf runs on a PRIVATE frame buffer (`run_jit_leaf` builds its own) — a helper-written frame slot would target the caller's frame, and the strict unmapped form's `vm.call_args` is only filled by `setup_certified_frame` on the non-leaf path. That exclusion is the fix for the `--jit`-only failure clusters (Object/defineProperty, Object/create, arguments-object, Array.prototype.*, Date — strict `(function() { return arguments; })()` IIFEs returned `undefined` on the machine-code path). |
| 34 | *(worktree)* | **Super property access** (Cut 61): certification now accepts `super` in non-arrow method/accessor bodies (`allow_super = allow_this && !is_class_constructor` — a body observing `super` also gets the `this` slot, the receiver); class constructors (`super()` + the this-before-super() TDZ) and arrows capturing `super` (lexical) stay env-path. The 12 super steps lower to helpers mirroring the interpreter handlers, branching the this binding / super base between the certified frame slot + home-object prototype and the env-path Function env: `GetSuperBase`/`ThisValue` (the base/receiver pair — `GetSuperBase` runs the this-binding check first, spec 13.3.7.1), `GetSuperName`/`GetSuperComputed`/`GetSuperComputedKeep` (reads through the base with the current this as receiver; the Keep converts the key once, writes it at the passed sp, and returns the value — `[base, key, base, key]` → `[base, key', value]`, the base surviving from the `GetSuperBase` capture), `AssignSuperName`/`AssignSuperComputed` (plain + compound writes), `UpdateSuperName`/`UpdateSuperComputed` (the `++`/`--` forms — interpreted but NOT emitted by the compiler: updates route through `ResolveSuperRef*` + `GetVarReference` + `UpdateVarReference`), `DeleteSuper` (the always-ReferenceError, spec 13.5.1.2 step 4.b — thrown before the key evaluates), and `ResolveSuperRefName`/`ResolveSuperRefComputed` (the logical-assign/update reference resolution — the reference records the base + this receiver). The base/receiver resolve through `current_function` (`certified_this` reads the `this_slot`, `certified_super_base` the home object's prototype — a static method's base is the superclass constructor); the async driver sets it for the whole run. ALL 12 steps are leaf-excluded — `GetSuperComputed` was MISSING from `steps_are_leaf`, and an inlined leaf would see the caller's this/home object (the sweep gap this cut closed). `SuperCall`/`GetVarReferenceThis`/bare `super(...)` stay bailed. |
| 35 | *(worktree)* | **Misc close-out** (Cut 62): the last bail-list row — `new.target` now CERTIFIES (the `FastScopeScan` accepts the `new.target` MetaProperty in non-arrow bodies — an arrow's `new.target` is lexical and `import.meta` stays env-path) and reads the new per-run `Vm::current_new_target` on the certified path (the frame-slot model creates no FunctionEnv to carry it; the certified construct path sets it, a normal call/driver run reads `undefined` — matching the async/generator drivers' hardcoded `undefined` FunctionEnv). The `NewTarget` step keeps its deliberate leaf exclusion (a leaf's new.target differs from the caller's construct context) but now compiles in real bodies via the general path. A direct-eval `CallFast` site no longer bails: `call_slow` threads the `direct_eval` flag to `do_call_fast` (whose `fast_call_core` routes a real `%eval%` callee through `perform_eval` with the caller's environment intact) — the compiler still never emits one (direct eval always takes the vector form), so the lowering is defense-in-depth. Heap-value constants (a `Push(Value::String/BigInt)` and the register path's `LoadConst`) now EMBED their NaN-boxed pointer bits instead of a `push_const`/`load_const` helper call — sound because the GC never moves boxes (the `Gc` handles are Copy) and the value outlives the code: the step holds it, the compiled body is traced (the function-site cache, or the active-run tracer for a script body), and the cache entry that frees the code also drops the body. |
| 36 | *(worktree)* | **Fused global/slot call lowering + certified-script JIT routing** (Cut 65): the last call-step bail rows — `CallFastGlobal` (a statically-known global-name callee at a stable cell), `CallFastSlotStore`, and `CallFastGlobalStore` (the fused `x = f(args)` stores) — now lower: the global-cell read feeds the existing leaf-probe call machinery, and a fused store materializes the arg slots (TDZ-checked in order) over the working region, calls, then TDZ-checks and stores the result to the target slot (`emit_call` gained an `emit_fall_through` so the store appends after its merge instead of bailing the body). A certified SCRIPT — previously always interpreted, since `eval_program` ran `vm.start` — now routes through `run_jit_body_loop`, so the top-level bench loops (`n = f(n)` fused global call-stores) execute in machine code with the leaf callees in-frame. Scripts complete through the interpreter's completion REGISTER (`vm.completion`/`completion_is_empty`), which the machine code now writes on the completion steps — `SetCompletion`, `ResetCompletion`, and the statement-position `FusedStoreLocal`/`FusedStoreGlobal` stores — via two offset constants (`runtime::jit::VM_COMPLETION_OFFSET`/`_IS_EMPTY_OFFSET`); the script path converts `run_jit_body_loop`'s fall-off-end `Return(undef)` marker back to the register's `Normal`/`Empty` completion (`Empty` ≡ `Normal(undefined)` at the top, so `NormalizeCompletion`/`ListBegin`/`ListEnd` stay no-ops — the register is unobservable mid-run in a certified body, and the interpreter's ListEnd-restore always lands on the value the JIT's never-reset register already holds). The compiled writes are null-guarded (the scaffold's bare-ctx test harness passes a null `vm`) and GC-safe (`Vm::trace` covers `completion`; the vm is an active-run root). |
| 37 | *(worktree)* | **Instantiation-machinery fast path** (Cut 66): the per-closure function/prototype boilerplate and the lookups around it — `set_function_properties` (`length`/`name`), `make_constructor` (`constructor`/`prototype`), and the restricted `caller`/`arguments` now append through a new `JsObject::fresh_data_define_attrs` (an explicit-attributes sibling of `fresh_data_define`: the descriptor clone + ValidateAndApplyPropertyDescriptor table are skipped, and the map transition encodes the non-default writable/configurable flags so the map read path serves them); the function-creation prototype intrinsics (`%Function.prototype%` + the generator/async variants) are cached per realm in a fixed array (the table's `get` built a JsString per call); `set_function_prototype` reads the generator/async flags from the caller instead of a second `ecma_functions` lookup; the body-site cache keys hash the source slice without a `JsString` allocation (`source_hash_at` — `shared_arrow_body` recomputed the key — clone + slice alloc + hash — on every closure); and `Map::transitions` (the shape forks on every property append) uses the Fx hash instead of SipHash. A closure-creation loop measures ~20% faster (the 100K×2 recursive bench ~0.49s → ~0.38s); the `{ a: 1 }` literal is already on the `create_data_property` fast path (~0.3µs — the report's older ~17µs predates it). The `ecma_functions` insert and the per-closure object allocations stay the residual floor. |
| 38 | *(worktree)* | **Inline small strings** (Cut 67): a `JsString` of at most 16 code units now stores its units INLINE in the box (`JsString::Small { len, units }`) — one arena allocation instead of the Vec + Arc + box the `Flat` path paid for tiny strings. `from_utf16`/`from_utf8` route small inputs to the inline form (larger ones keep the Arc-backed `Flat`), `concat`'s small path builds the inline form (the previous Vec + `Arc::from(Vec)` + `Flat` triple allocation is gone), and the leaf-merge branch accepts `Small` operands (a 17-128 unit leaf-leaf concat still merges to an Arc-backed `Flat`). The box is stable (the arena never moves boxes), so `as_slice`'s borrow of the inline units is valid while the owning handle is alive. Measured on the independent-small-concat shape (`s = x + x` with a 2-unit operand, 100K): ~0.033s → ~0.026s in BOTH the interpreter and the JIT (~20% — the JIT's `concat_strings` helper call and the interpreter's `apply_binary` share the concat); the rope append chain (`s += 'x'`) is unchanged (its ConsString node path was already a single allocation per append). |
| 39 | *(worktree)* | **Leaf-cache revalidation on a stale epoch** (Cut 68): a compiled leaf-call site whose cached verdict misses on a stale leaf-eligibility epoch (a "disturbing" helper — a getter, `valueOf`/`toString`, a nested call — bumped `ctx.leaf_epoch` since the probe) now re-validates that the eligibility state is AT REST — all seven `Vm::can_inline_leaf` control stacks empty, `env_stack` length 1, realm count 1 (`emit_leaf_state_at_rest`) — and if so re-stamps the cache's epoch and reuses the cached verdict, instead of re-running `leaf_call_probe` every iteration (the monomorphic-hot-call-next-to-a-getter shape previously probed per iteration: the probe count drops 100K → 1 on the getter and bool-add benches). The revalidation is sound because the cached `LeafInlineInfo` is a pure function of the callee (identity-checked) and the at-rest state: a negative cached verdict → `call_slow` is always correct, and a positive one is correct when the state is at rest again. The gate stays strict: a stale epoch on a DIFFERENT site/callee still probes. The cache itself is now a direct-mapped set of `LEAF_CALL_CACHE_ENTRIES = 4` records (indexed by `(index ^ (index >> 2)) % 4` — the xor-fold breaks the `% 4` collision on call sites spaced 4-8 steps apart, which the fused call-store statements produce; a colliding pair is just a re-probe, the exact `site` check still gates every reuse), so two hot call sites each keep a warm verdict (the two-hot-sites bench's ~200K probes drop to 2). The compiled check reads the stack lengths through new `pub const` offsets (`runtime::jit`), using the structural invariant `len == 2 * size_of::<usize>()` for the private `Vec` len fields. |
| 40 | *(worktree)* | **JIT compile threshold** (Cut 69): the corpus's one-shot bodies pay a Cranelift compile (~100µs each) they never amortize, so `lookup_info` — the single choke point behind `run_jit_body`/`run_jit_resume`/`run_jit_leaf`/`leaf_call_probe` — now gates straight-line bodies (`CompiledBody::has_loop` false) behind a consult counter (`CompiledBody::jit_calls`, saturated; `JIT_COMPILE_THRESHOLD = 16`): they run interpreted until the count reaches the threshold, then compile on the next consult (the count is deliberately NOT cached in `jit_info`, so promotion is never blocked and the `1` sticky-unsupported mark is only written at/after the threshold). Loop bodies (`body_has_loop` — a back edge per the `step_targets` mirror, a `FastLoopHead`/`RunRegBody` step, or a `TailCallSelf*` step, whose interpreter TCO loop re-enters the body without re-consulting the JIT) and hot leaves under a compiled caller (the probe caches a rejection; `call_slow` → `try_jit_leaf` → `run_jit_leaf` re-consults per call) still promote, so the Cut 65 benches are preserved. The consult count aggregates across the `Rc` clones of one declaration site. |
| 41 | *(worktree)* | **Vector-call leaf-inline + the apply/call leaf fast path**: the interpreter's vector-form call (`Step::Call`, the ≥9-arg/spread form) was the last "vector call to a certified LEAF runs the general `call_inner`" gap — `do_call` now rebuilds the fast-form layout and routes through `do_call_fast` (the same leaf-inline core the JIT's `call_vector` helper already used, Cut 50), so a certified leaf runs inline on the caller's Vm with no execution-context push or fresh-Vm round trip (interp 53.2ms → 21.3ms on a 9-arg-leaf row). `Function.prototype.apply`/`call` to a certified leaf previously went `crate::function::call` → `ordinary_call` (the general path); `try_leaf_call` now runs the leaf machinery on a POOLED Vm (register-op or JIT body execution, no context push), gated like `fast_call_core` (single realm, EcmaScript, `leaf_lookup` hit — `leaf_inline` excludes class constructors, so `C.apply(null, [])` still throws), and registers the pooled Vm as an `ACTIVE_RUNS` entry for the leaf window (`with_leaf_run`) so a budget collection inside the body traces it like `run_inner` (verified under `--gc-stress`). It deliberately does NOT route through the running Vm: the caller's `&mut Vm` and an args slice into its stack are live across the native-handler call, so mutating it through a raw pointer would be UB. A/B (3 runs): interp 235ms → 208ms, jit 223ms → 204ms. New `--jit-bench` rows (`vector leaf call`, `apply leaf call`); docs updated in perf.md. The remaining apply floor (~1µs/call — the builtin round-trip + per-call arg-list build) needs call-site `.apply` inlining, a compiler slice. |
| 42 | *(worktree)* | **Allocation-free typed-array element encode + the compiled length probe**: the last two typed-array levers. `encode_element`'s per-element `Vec<u8>` is replaced by `encode_element_into` (a stack `[u8; 8]` buffer returning the used length) in every per-element write path — `typed_array_element_set`, `typed_array_set`, the index define, Atomics' `element_raw`, and `fill`'s encode-once — so the JIT store helper inherits it through `typed_array_element_set` (JIT write row 51.5ms → ~30ms, interp ~92ms → ~73ms). A new `typed_array_length` helper (a pure `(ctx, object)` probe: the slots length for an IntegerIndexed receiver, the canonical-NaN sentinel otherwise — whitelisted as leaf-eligibility-neutral, `emit_raw_call`) fronts the compiled `GetMemberName`-with-`length` site inside `emit_member_cell_read` before the member-cell probe, skipping the `get_member_name` FFI round-trip on a hit and falling through for every other receiver (JIT length row 19.5ms → ~11.3ms; the sentinel is exact — a length is never NaN). New e2e + direct unit tests; full sweep green. |
| 43 | *(worktree)* | **Per-statement register-run segmentation + the register counter/member fixes**: a loop body containing control flow no longer bails the WHOLE body to the step path — `lower_leaf_ops_segmented` splits the compiled body steps into maximal straight-line runs (each lowered by the shared per-step `lower_step`, the whole-body `lower_leaf_ops` now a wrapper over it) and replaces each run with its own `Step::RunRegBody` (`apply_register_runs` re-bases the labels/fixups; a label landing strictly inside a run keeps the body unsegmented). The JIT needed no changes — its `RunRegBody` arm already truncates the working stack to the entry depth (`error_sp` for the try path) and falls through to the next step. Three soundness fixes surfaced by the segmentation: (1) the list wrappers stay paired on the step path (an absorbed `ListBegin` with a step-path `ListEnd` pops the enclosing block's completion entry — nested loops in blocks restored stale completions); (2) a run must not start at a `SetCompletion` (absorbing the pop without the statement's steps leaves its value on the stack — a per-iteration drift in the compiled path, where `max_stack_usage` pre-sizes the working area: `o[i] = i; b['x'] = i` crashed with a Vec-corruption panic); (3) `Step::trace` now walks the `RunRegBody` ops' literal values (`trace_leaf_op_heaps`), so a member store's `Const` key string is rooted under `--gc-stress` (the box was swept mid-run and read back as garbage). Also fixed: the interpreter's `RegOperand::Counter` member-operand resolution read the loop counter from the VALUE STACK (a stale pop since Cut 35 slice 21 removed the entry push — the JIT already read the field); it now reads the dedicated `loop_counter` field, so `a[l++] = i`/`b[j] = j*10` store the right values in the interpreter (the committed spec test's scripts never certified, so the pop went unseen). Measured: `buildString shape` ~193.8ms → ~181.9ms (~6%, A/B medians of 3). |
| 44 | *(worktree)* | **Dense-array argument lists for the compiled `CallApply` step**: the `do_call_apply` handler's `Apply` arm now recognizes a dense Array argArray and pushes its elements straight onto the value stack — no per-call `Vec` allocation, no `length` property path / `ToLength` round trip, no `[[Get]]`-loop element reads (the compiled `call_apply` helper inherits it with no JIT changes). The gate is exact: a hole, a length past the buffer end, or a non-Array falls back to `create_list_from_array_like` (whose own fast paths and the spec `[[Get]]` loop keep those shapes unchanged), and the buffer borrow never spans the call (a re-entrant callee may mutate the argument array). Measured on the `apply leaf call` row (200K, A/B medians): interp ~101ms → ~57ms, jit ~98ms → ~50ms (~1.8x / ~1.9x). New spec-exact unit test; Function/prototype/apply+call fixtures clean under `--gc-stress --jitless`; full sweep green. |

### Slow-path helper table (`JitSlowPaths`, 113 helpers)

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
`array_hole`/`array_end` (the array-literal steps, Cut 52),
`object_begin`/`object_init_name`/`object_init_computed`/
`object_key_to_property_key`/`object_method_name`/`object_method_computed`/
`object_accessor_name`/`object_accessor_computed`/`object_spread` (the
object-literal steps, Cut 53), `push_str`/`concat_str`/`concat_str_const`/
`push_const` (string literals + template concat, Cut 54), `load_const`
(a register body's heap constant, Cut 54), and the Cut 55 try/control
set: `enter_block`/`leave_block` (block envs), `enter_try`/`exit_try`
(`TryFrame` push + the normal `Exit` transfer), `return_control`/
`break_control`/`continue_control`/`throw_control`/`finally_end` (the
abrupt transfers, returning a step target or a completion sentinel),
`catch_bind` (the catch parameter binding), and `dispatch_error` (the
pending-engine-error handler dispatch); the Cut 56 switch pair
`switch_disc`/`switch_test` (the stored discriminant and the strict-
equality case test); and the Cut 57 iterator set: `for_in_begin`/`for_in_next` (the for-in enumeration open + advance — the key lands at the passed working-stack pointer, the return is 1 = key / 0 = done),
`for_of_begin`/`for_of_next`/`for_of_next_bind_local` (the for-of open +
advance, sharing `Vm::for_of_advance` — the fused bind writes the frame
slot directly), `for_of_close` (the boundary pop + generic close),
`for_of_close_all` (the non-try engine-error close, mirroring `run_inner`
and skipping when `for_of_stepping`), and `enter_per_iteration`/
`per_iteration` (the certified loop's per-iteration envs, step-index
helpers); and the Cut 58 suspension pair `yield_suspend`/`await_suspend`
(the `Yield`/`Await` helpers that record the suspension — value, delegate
flag, working-stack pointer, continuation step — and signal
`DISPATCH_SUSPEND`); and the Cut 59 destructure set:
`destructure_begin`/`destructure_next`/`destructure_rest`/
`destructure_close` (an array pattern's iterator: open, step — returning
the element bits with the exhausted-iterator `undefined`, collect-rest —
pop without close, close — pop-before-close), `destructure_obj_coercible`/
`destructure_obj_key`/`destructure_obj_key_computed`/
`destructure_obj_key_store`/`destructure_obj_key_get`/`destructure_obj_rest`/
`destructure_obj_end` (an object pattern's RequireObjectCoercible open, key
reads — constant key via the step payload, computed via the popped value —
the store/get computed-key pair, the rest copy with the merged exclusion
set, and the close), and `destructure_close_all` (the engine-error close,
mirroring `run_inner`'s uncovered-error close and skipping when
`destructure_stepping`); and the Cut 60 pair `create_arguments` (the body's
`arguments` object — sloppy mapped, aliasing the formals through the
capture context, or strict unmapped — stored into the frame slot, a
step-index helper) and `typeof_top` (a `typeof` value operand's string;
pure); and the Cut 61 super set: `get_super_base`/`this_value` (the
base/receiver — `get_super_base` runs the this-binding check first, spec
13.3.7.1), `get_super_name`/`get_super_computed`/`get_super_computed_keep`
(reads through the base with the current this as receiver — the Keep writes
the converted key at the passed sp and returns the read value),
`assign_super_name`/`assign_super_computed` (plain + compound writes),
`update_super_name`/`update_super_computed` (the `++`/`--` forms —
interpreted but not emitted by the compiler), `delete_super` (always the
ReferenceError, spec 13.5.1.2 step 4.b), and `resolve_super_ref_name`/
`resolve_super_ref_computed` (the logical-assign/update reference resolution
recording the base + this receiver). A body needing a `None` helper bails to the
interpreter.

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
0 crash / 0 hang** (3 skip); `annexB` 1086 pass / 0 hang; `built-ins` 23231
pass / 426 hang — *pre-existing* (baseline 447± load wobble) slow RegExp
property-escape / CharacterClassEscapes fixtures plus the known
Temporal/TypedArray hang clusters, unrelated to the JIT (0
fail / 0 crash). Clusters: `expressions/call` 92/92, `arrow-function`
343/343.

**Tests**: `cargo test --workspace` green (4522 passed, 3 ignored; jit crate 149 incl. the
`installed_jit_*` e2e tests for member/slot callees, loop-with-calls, mid-run
cell mutation, GC-stress rooting, global store-then-read, scope-shadow
correctness, the Cut 55 try/catch/finally/block shapes, the Cut 56
switch/fall-through/nested/hot-loop shapes, the Cut 57
for-of fast/generic/hot-loop, for-in keying, holes + break/continue,
break/return/next-error/body-error iterator closing, captured-head
per-iteration envs (for-of AND for-in), for-of-in-try shapes, the Cut 58
async-await/await-in-fast-loop/async-rejection-catch/async-try-finally,
generator-plain-yields/yield-in-fast-loop/generator-throw-catch,
async-generator-interpreter-fallback, and async-method-returns-nested-
async-function (the capture-env-fallback regression) shapes, the Cut 59
array/object declaration destructure (defaults, rest, nested, computed
keys, captured names), assignment destructure, destructure-in-a-loop,
iterator-close on completion, and the next-error-keeps-iterator-open
shapes, and the Cut 60 sloppy mapped arguments (index reads, aliasing both
ways, `arguments.callee`, strict unmapped non-aliasing, captured params
shared with a closure) and strict-unmapped-returned-object (leaf-frame
regression) shapes, the Cut 61 super shapes (calls, computed read/call
stack shapes, assign/compound, prefix/postfix updates, logical assign,
async method, delete ReferenceError, static/inherited base, read-vs-call
shapes), and the Cut 62 misc shapes (`new.target` normal-call/construct,
heap string loop, heap bigint leaf, direct-eval `CallFast` lowering), and
the Cut 65 fused-call + script-completion shapes (slot/global fused
call-stores, a certified script's whole top-level loop in machine code,
and an interpreter-match matrix of script completions — var-declaration
empty, statement-position assignment, if with/without a value, trailing
empty block restore, the Reset/Normalize interplay, and the fused
call-store literal-arg vs counter-path shapes), and the Cut 66
instantiation shapes (a crux test for `fresh_data_define_attrs` — explicit
writable/enumerable/configurable flags encoded in both the map descriptor
and the property vector, the map read path agreeing, a non-writable write
rejected, and a configurable re-define mirroring into the inline field),
and the Cut 67 small-string shapes (a crux test for the inline form — a
small input routes to `Small`, `len`/`as_slice` read it, the 16-unit
boundary spills to `Flat`, a small concat result is `Small`, a 17-unit
concat result is `Flat`), and the Cut 68 stale-epoch leaf-cache
revalidation shape (a getter next to a hot leaf call in a 100K-iteration
loop, with a counting probe wrapper proving the site probes once and
re-stamps the epoch instead of re-probing per iteration), and the Cut 68
two-hot-sites shape (two leaf call sites in a 100K loop, each warming its
own direct-mapped record — 2 probes total));
`cargo clippy --workspace --all-targets -- -D warnings` clean. The Cut 63
script/eval compiled-body cache is covered by a runtime test
(`script_and_eval_bodies_cache_per_source`).

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
- **Env machinery**: `EnterWith`, `PerIteration`/`EnterLoopEnv`/
  `EnterPerIteration` (creation), `UsingInit`/`DeclInit`. The block-env steps
  (`EnterBlock`/`LeaveBlock`) and the whole try set (`EnterTry`/`Exit`/
  `CatchBind`/`FinallyEnd`) are now **compiled** (Cut 55), along with the
  abrupt transfers `Return` (in a try body), `Break`/`Continue`/`Throw` —
  see the implemented table.
- **Iterator machinery**: `AsyncForOf*` only — the certified `ForIn*`/
  `ForOf*` steps and the per-iteration env creation
  (`EnterPerIteration`/`PerIteration`) are now **compiled** (Cut 57); the
  env-path head binds (`ForInBind`/`ForOfBind`) and restores
  (`ForInRestore`/`ForOfRestore`) stay on the env path, and async for-of
  never certifies.
- **Suspension**: `YieldStar*`/`AsyncYieldStar*` only — a `yield*` body
  never certifies (certification rejects the delegate form) and async
  generators (both flags) never certify; plain `Yield`/`Await` in a
  generator/async-function body are now **compiled** (Cut 58) — see the
  implemented table.
- **Control machinery**: class steps (`ClassBegin`/
  `ClassHeritage`/`ClassFinish`…), `RegExpLiteral`. `SwitchDisc`/`SwitchTest`
  are now **compiled** (Cut 56) — see the implemented table.
- **Destructuring/spread**: the certified `Destructure*` steps are now
  **compiled** (Cut 59) — see the implemented table. `ObjectSpread`/
  `ArraySpread` (the literal-spread steps) were compiled in Cuts 52/53. The
  env-path wholesale binds (`Destructure { pattern }`/`DeclInit { pattern }`)
  stay on the env path (a certified body always compiles patterns to the
  primitive steps with flat slot/context binds), as do destructuring-assign
  targets with member elements (the reference machinery), `SetFunctionName`
  (anonymous-function defaults), and `UsingInit`.
- **Sloppy mapped arguments**: `CreateArguments { mapped: Some }` is now
  **compiled** (Cut 60) — see the implemented table; `TypeofIdent` (the
  unresolvable-reference `typeof` form) stays on the env path.
- **Super machinery**: `SuperCall` (bare `super(...)` constructor calls) and
  `GetVarReferenceThis` only — the property-access steps (`GetSuperBase`/
  `ThisValue`, `GetSuperName`/`GetSuperComputed`/`GetSuperComputedKeep`,
  `AssignSuper*`, `UpdateSuper*`, `DeleteSuper`, `ResolveSuperRef*`) are now
  **compiled** (Cut 61) — see the implemented table. Class constructors and
  arrows capturing `super` never certify.
- **Misc**: `ThisValue` (only ever emitted alongside the super steps — the
  exclusion is redundant) and `NewTarget` (a deliberate leaf exclusion: a
  leaf's `new.target` differs from the caller's construct context) — both
  compile on the general path, and `new.target` now certifies (Cut 62) —
  see the implemented table. Direct eval never reaches a `CallFast` site
  (the compiler forces the vector form); the JIT lowers one anyway (Cut
  62). Heap-value constants inline their bits (Cut 62).

**The `buildString` harness loops (the sweep's slowest cluster)** — the
`RegExp/property-escapes/generated` + `CharacterClassEscapes` fixtures
(previously 378-432 sweep "hangs") spend their time NOT in the regexp
engine (the regex tests themselves measure 0-17 ms; the engine passes the
cluster's 1500+ runnable fixtures) but in the vendored harness's
`buildString` (`test262/harness/regExpUtils.js`), which fills a JS array
in a loop and calls `String.fromCodePoint.apply`. The body DOES certify
and compiles (`lookup_info` succeeds — the array store / member write /
`.apply` steps all have JIT arms), but every op lowers to a slow-path
helper call, so the machine code runs at interpreter speed (`--jit`
changes nothing on these loops).

The cost profile (release build, measured) had three layers; the first
two are fixed (2026-08-31) and the whole cluster now passes clean:

1. **The per-store interner lock** on the canonical index string
   (`PropertyKey::from_utf8(&i.to_string())` → process-wide `Mutex`):
   ~415 ns/op, 94% of the prototype-less store cost. Fixed by
   `crux::string::index_atom` — a thread-local index → atom memo
   (`PropertyKey::from_index`) — measured 8× on the crux append; the
   fixtures' chunked `buildString` reuses ~10k indices, so every store
   after the first chunk hits the memo lock-free.
2. **The `arr.length = 0` reset was quadratic** — `array_set_length`
   deleted per-element, and each `delete_key` is an O(n) scan + O(n)
   remove + index-map null (the next lookup rebuilds it), so resetting a
   10k-element array cost O(n²) per chunk. Fixed by a dense-truncation
   fast path (one `SmallProps::truncate` when the own properties are
   exactly `["0".."old_length-1", "length"]` and all elements are
   configurable data). `buildString` of a 2,162,560-unit string:
   ~3.4 s → ~0.65 s (~5×), and the fixtures 6.7 s → ~1.4 s.
3. **Remaining**: the interpreter's per-step dispatch (~90 ns/step — the
   buildString inner loop is ~13 steps) and the JIT's per-op `call_slow`
   round-trips (§7 item 12). The store loop is now ~45% faster overall
   (measured on the fixture's exact shape: 3M `codePoints[length++] =
   codePoint` iterations, ~1670 ms → ~920 ms interpreted; the JIT inline
   store adds a further edge under `--jit`), the whole `built-ins/RegExp`
   cluster passes **1878/1879 with 0 fail / 0 crash** (batch-4 sweep; the
   single residual hang, `RegExp/character-class-escape-non-whitespace.js`,
   is a 65,536-iteration `\S+` replace loop — a REGEXP ENGINE cost, not
   buildString), and the full 48,622-fixture sweep is **48130 pass / 0 fail /
   0 crash / 334 hang** (down from 374 hangs: the batch-32 sweep's per-BATCH
   15 s deadline still kills 32-fixture batches of ~1-2.5 s fixtures — each
   fixture passes individually; the sweep runs interpreted by default,
   `test262-sweep --jit` is opt-in).

## 7. TODO / next steps

**Correctness/soundness gaps (same shape as the fixed bug):**

1. ~~**`instantiate_accessor`**~~ — **done** (Cut 48): the accessor body
   `Rc<Block>` is now shared per site (`shared_accessor_body`, mirroring the
   Cut 43 function/arrow cache), so the compiled IR + JIT code compile once
   per getter/setter site instead of per instantiation. Measured on an
   accessor-in-a-loop with a per-iteration call: `--jit` 1100ms → ~82ms
   (~13×) and the interpreter 120ms → ~80ms (the per-instantiation IR
   recompile was hurting both paths).
2. ~~**Script/eval bodies**~~ — **done** (Cut 63): `eval_program` now
   consults a per-agent `script_bodies` cache keyed by the exact source text
   + (strict, fast_script) — re-evaluating the same source reuses the
   compiled IR (the parse and the per-eval declaration instantiation still
   run; the compile — and the JIT machine code via the shared per-body
   `jit_info` fast pointer, should scripts ever JIT — do not repeat). Sound
   because the eval parse context (`in_function`/`in_method`/private names)
   only gates whether a source parses (early errors), never the AST it
   produces, and a direct eval's caller-inherited strictness is part of the
   key. Repeat-eval-in-loop (an `eval(src)` in a loop) no longer recompiles
   per iteration.
3. ~~**`params.clone()` per closure instantiation**~~ — **done** (Cut 64):
   `EcmaFunction.params` is now a shared `Rc<[BindingElement]>`
   (`shared_params`, keyed by the shared body `Rc<Block>` — the canonical
   site identity), so instantiating a closure in a loop (and the uncertified
   call/TCO paths that read the record) no longer deep-clones the param
   list per closure/call. Readers take `&[BindingElement]` via deref
   coercion unchanged.

**Performance (the remaining benchmark rows and hot shapes):**

4. ~~**Inline direct leaf calls further**~~ — **done** (Cut 65): the fused
   global/slot call steps (`CallFastGlobal`, `CallFastSlotStore`,
   `CallFastGlobalStore` — a statically-known leaf callee at a stable slot
   or global cell, previously bailing the body) now lower through the
   leaf-probe call machinery, and a certified script routes through the JIT
   for the first time (`eval_program` runs `run_jit_body_loop`, which
   required the compiled completion steps to write the interpreter's
   completion register). The leaf probe still runs per call site — skipping
   it for the statically-known case is future work.
5. **Closure-creation lowering** (`Step::Closure`/`CreateFunction`/
   `CreateArrow`) — **done** (Cut 44): the creation step now compiles via
   step-index helpers; the remaining cost is the instantiation machinery
   itself (env + object allocation), which a future inline fast path for
   capture-free closures could cut.
6. ~~**The instantiation machinery is the floor**~~ — **done** (Cut 66): the
   fresh-function/prototype boilerplate (`length`/`name`/`prototype`/
   `constructor` and the restricted `caller`/`arguments`) now appends via a
   new `JsObject::fresh_data_define_attrs` fast path (explicit-attribute
   sibling of `fresh_data_define` — the descriptor clone + the
   ValidateAndApplyPropertyDescriptor table are skipped, and the map
   transition encodes the non-default attributes), the function-creation
   prototype intrinsics (`%Function.prototype%` and the generator/async
   variants) are cached per realm in a fixed array (the table's `get`
   allocated a JsString per call), `set_function_prototype` takes the
   generator/async flags from the caller instead of a second `ecma_functions`
   lookup, the body-site cache keys hash the source slice without building a
   `JsString` (`source_hash_at` — `shared_arrow_body` recomputed the key on
   every closure), and `Map::transitions` uses the Fx hash (SipHash on every
   shape transition). Measured on the closure benches: the recursive
   closure loop (100K levels × 2 closures) ~0.49s → ~0.38s (~20%), a
   200K create-only loop ~0.32s → ~0.29s; the `{ a: 1 }` literal is already
   ~0.3µs (the old ~17µs figure predates the `create_data_property` fast
   path). The `ecma_functions` insert and the per-closure object
   allocations remain the residual floor.
7. **Extend the bail list**: `with`, `using`,
   iterators, generators/async, destructuring/spread, class machinery, mapped
   `arguments` — each is a slice of lowering + helper work. Proper tail calls
   are **done** (Cut 45); the NAMED (Cut 46), global-name (Cut 47), and
   **vector-form** (Cut 51 — a spread or >8 plain args now rebinds the frame
   from the Vm's argument vector and jumps) self-tail-call forms run in
   machine code; the **vector call form** (Cut 49) is done, and **array
   literals** (Cut 52 — the five array steps lower, with a dense-array fast
   spread in the spread helpers), **object literals** (Cut 53 — all nine
   steps lower, methods/accessors via step-index helpers), **string
   literals** (Cut 54 — `Push(Value::String)`/`PushStr`/the template
   concat steps and register-body heap constants lower; the step-index
   forms are excluded from leaf-inlining because an inlined leaf's helpers
   would see the caller's `JitCallContext::body`), **try/catch/finally**
   (Cut 55 — certification now accepts `try`, the try/block/catch/finally
   steps and the `Return`/`Break`/`Continue`/`Throw` transfers lower through
   the control-dispatch helpers, and engine errors in a try body route
   through the handler table), **switch** (Cut 56 — the discriminant
   stores via `switch_disc`, each `SwitchTest` strictly-equals a case test
   and the machine code jumps to the matched case block; a case body with a
   direct Annex B block function declaration stays on the env path), and
   **for-in/for-of + per-iteration envs** (Cut 57 — the certified
   `ForIn*`/`ForOf*` steps lower through the iterator helpers, the
   do-while back edge jumps in machine code, and a captured lexical head's
   `EnterPerIteration`/`PerIteration` envs compile; the env-path head binds
   and restores, and async for-of, stay on the interpreter), and
   **generator/async suspension** (Cut 58 — plain `Yield`/`Await` in a
   certified generator/async-function body lower to the suspension
   helpers; the entry dispatch on `resume_kind` resumes at the continuation
   or routes a throw/return through the control machinery, the working
   region survives in the traced `Vm::jit_work`, and the drivers install
   the capture context + frame; `yield*` and async generators stay on the
   interpreter), and **destructuring** (Cut 59 — the certified primitive
   `Destructure*` steps lower through the iterator/object helpers with flat
   slot/context binds; a destructuring-assign target with member elements,
   the env-path wholesale binds, `SetFunctionName`, and `UsingInit` stay
   on the interpreter), and **arguments objects** (Cut 60 —
   `Step::CreateArguments` lower for both the sloppy mapped and strict
   unmapped forms; the mapped object aliases the capture-context params and
   reads `arguments.callee` from the running context, plus `TypeofTop` for
   `typeof` value operands), and **super property access** (Cut 61 — the
   12 super steps lower through the base/receiver helpers; the this binding
   and super base read the certified frame slot + home-object prototype via
   `current_function`, while class constructors and arrows capturing
   `super` stay on the interpreter), and **`new.target` + the last misc
   row** (Cut 62 — `new.target` certifies and reads the per-run
   `current_new_target`; a direct-eval `CallFast` site lowers through
   `call_slow`; heap-value constants embed their bits) no longer bail, so a
   spread self-tail-call compiles and jumps end to end (~430ms for 200K,
   down from the ~2s interpreter bail; the array-creation machinery is the
   remaining floor). Vector calls to certified leaves now leaf-inline on
   BOTH paths: the JIT's `call_vector` already routed through `do_call_fast`
   (Cut 50), and the interpreter's `do_call` now rebuilds the fast-form
   layout and does the same, so the "vector call to a certified LEAF still
   runs the general `call_inner`" gap is closed.
   `Function.prototype.apply`/`call` to a certified leaf run the same leaf
   machinery (`try_leaf_call` on a pooled Vm — see the implemented table);
   the remaining apply floor (the builtin round-trip + per-call arg-list
   build, ~1µs/call) is now **done** (2026-09-01): the compiler recognizes
   `f.apply(x, arr)` / `f.call(x, ...)` member calls and emits the new
   `Step::CallApply`, which the JIT lowers through the `call_apply` slow-path
   helper — the resolved function is checked against the realm's intrinsic
   and the call runs directly on the Vm (leaf-inline included), with the
   shadowed/array-like/general cases falling back to the interpreter's
   `do_call_apply` exactly. A computed
   callee (`getF()(n-1)`) still pays the per-iteration `tail_call` helper
   round-trip.
8. ~~**String concat**~~ — **done** (Cut 67): a string of at most 16 code
   units now lives INLINE in the `JsString` box (`JsString::Small`), so a
   small literal and a small concat result are a single arena allocation
   instead of a Vec + an Arc + the box. `from_utf16`/`from_utf8` and
   `concat`'s small path build the inline form; the concat's leaf-merge
   branch accepts `Small` operands (17-128 units still merge to an
   Arc-backed `Flat`). Measured on the independent-small-concat shape
   (`s = x + x`, 100K): ~0.033s → ~0.026s in both the interpreter and the
   JIT (~20%); the rope append chain (`s += 'x'`) is unchanged (its
   ConsString node path was already one allocation).
9. ~~**Leaf-cache invalidation breadth**~~ — **done** (Cut 68): a disturbing
   helper (a `valueOf`, a getter) still bumps `leaf_epoch` and invalidates
   the per-site leaf verdict, but the compiled call site now re-validates
   the eligibility state on the stale epoch: if it is at rest (the
   `can_inline_leaf` stacks + realm count), the site re-stamps the epoch
   and reuses the cached verdict instead of re-probing — the probe count on
   a monomorphic hot call next to a getter drops from ~100K to 1 per loop.
   A stale epoch on a DIFFERENT site/callee still probes; two hot call
   sites each cache separately in the direct-mapped `LeafCallSiteCache`
   set (a record per `(index ^ (index >> 2)) % 4` slot — a colliding pair
   just re-probes, since the exact `site` check still gates reuse).

**Non-JIT engine work observed along the way:**

10. **`built-ins` RegExp property-escape fixtures** — **root cause identified**
   (2026-08-31): the slowest cluster in the sweep is not RegExp cost (the
   regex tests measure 0-17 ms; the engine passes the cluster's 1500+
   runnable fixtures with 0 fail / 0 crash) but the vendored harness's
   `buildString`, whose array-store / member-write / `.apply` body never
   certifies and runs interpreted at ~100× mainstream speed — full
   measurements in §6. The hang counts wobble with load (378-432); all
   fixtures pass individually at idle. The fix is the certification work
   in item 12.
11. **`--gc-stress` is superlinear** on 100K-closure fixtures (per-allocation
    collection × per-iteration allocations) — inherent to the mode, not a
    regression.

**JIT compile threshold (Cut 69):** the corpus's one-shot bodies pay a
Cranelift compile (~100µs each) they never amortize, so `lookup_info` —
the single choke point behind `run_jit_body`/`run_jit_resume`/
`run_jit_leaf`/`leaf_call_probe` — now gates straight-line bodies
(`CompiledBody::has_loop` false) behind a consult counter
(`CompiledBody::jit_calls`, saturated; `JIT_COMPILE_THRESHOLD = 16`):
they run interpreted until the count reaches the threshold, then compile
on the next consult (the count is deliberately not cached in `jit_info`,
so promotion is never blocked and the `1` sticky-unsupported mark is
only written at/after the threshold). Loop bodies (`body_has_loop` — a
loop bodies (`body_has_loop` — a
back edge per `step_targets`, a `FastLoopHead`/`RunRegBody` step, or a
`TailCallSelf*` step, whose interpreter TCO loop re-enters the body
without re-consulting the JIT) and hot leaves under a compiled caller
(the probe caches a rejection, `call_slow` → `try_jit_leaf` →
`run_jit_leaf` re-consults per call) still promote, so the Cut 65
benches are preserved. The consult count aggregates across the `Rc`
clones of one declaration site. Remaining tuning: the K value (4/16/64),
and a step-count floor for tiny loops.

12. **Make the `buildString` shape fast (array stores + member writes)** —
   the work that cleared the ~380-430 RegExp fixture hangs (§6). The body
   already certifies; the bottleneck was per-op cost, and five slices
   fixed it (measured on the fixture's store shape, 3M `codePoints[length++]
   = codePoint` iterations with a 10k chunk reset: ~1670 ms → ~920 ms
   interpreted, ~45%):
   - the per-store interner lock on the canonical index string
     (`crux::string::index_atom` — a thread-local index → atom memo,
     `PropertyKey::from_index`, ~8× on the crux append);
   - the quadratic `arr.length = 0` reset (`array_set_length`'s per-element
     deletes with an O(n) scan/remove/index-map rebuild each → a
     dense-truncation fast path);
   - `SmallProps::truncate` DROPPED the heap Vec on every truncate-to-inline
     (the fill-reset-fill pattern re-paid the 3→12k Vec growth per chunk),
     and `SmallProps::push`'s spill allocated a fresh Vec each time —
     truncate now keeps the heap's allocation (`clear()` instead of
     `Vec::new()`) and the spill reuses it (measured ~220 ns/append →
     ~55 ns);
   - the reset's dense check re-parsed every key string through the
     interner (`array_index_of` → `lookup` → lock + alloc per key, ~4×
     slower) — it now compares the keys by ATOM against the memoized
     `PropertyKey::from_index(i)`;
   - the prototype-chain walk in `array_element_write` called
     `get_own_property_key` per link, which re-parsed the index string for
     the Array length fast path (~130 ns/append on a real
     Array.prototype → Object.prototype chain — the earlier "~33 ns"
     measurement was a prototype-less bench) — a parse-free
     `has_own_index_property` (the Array length shortcut + an atom-compare
     store lookup) drops the walk to ~50 ns.
   Plus the JIT inline store (slice 3, the old item (b)): `AssignMemberComputed`
   with a plain `=` now calls a direct `fast_array_element_write` helper
   (Array kind + canonical index Number check, then `array_element_write`;
   1 = stored, 0 = fall back to the full `assign_member_computed` helper;
   never errors — the chain walk bails on anything but a plain
   Ordinary/Array link, so no trap or getter can run). Measured ~2% on the
   store loop under `--jit` (the call_slow wrapper was small; the crux
   machinery was the cost), which is what the doc's earlier
   "chain-clean cache not justified" note predicted — the walk fix above
   was the higher-value piece. The residual fixture time is REGEXP ENGINE
   cost (`\p{...}` class handling, the `\S` replace loops), not the
   harness's array fills. Note the sweep runs interpreted by default, so
   the crux slices matter for the default-sweep floor; the inline store
   matters for the `--jit` mode.
