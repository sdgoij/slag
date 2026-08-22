# Bytecode for everything: the runtime IR, Ignition-style

This is the engineering spec for turning the compiled IR into a real
bytecode VM. It specifies the instruction-set delta, the scope-analysis
algorithm, the runtime integration points, and the quantified path to the
perf.md gate. The reference is the V8 checkout in `v8/` — the file/function
names below are the exact places to read.

Status: **Cuts 1-4's first slices landed and validated** (zero conformance
regressions each), and the perf gate is now **closed**: every `--bench` row
is ≥ 5x the corrected 2026-08-18 baseline (arithmetic 2.52s → ~43ms,
property 3.22s → ~83ms, string 0.88s → ~43ms, array 15.42s → ~233ms,
function calls 5.73s → ~293ms; see `docs/perf.md`). Cut 3 gives simple-param
functions and arrows compile-time binding resolution — params and `var`s
become fixed frame slots, so identifier ops emit
`LoadLocal`/`StoreLocal`/`InitLocal`/`UpdateLocal` instead of the
per-read environment-chain walk — and the follow-on script-level binding
mechanism (fast scripts) slots top-level `var`s too. Cut 4 fuses the loop
test and update into slot ops, adds a primitive fast path to the
relational evaluator, and (the continuation's first slice) runs a
canonical loop's counter in the VM accumulator. The Cut 3 continuation
has landed in slices: a certified body may create closures — first
capture-free, then (the latest slice) **capturing** closures, whose
captured bindings move into a per-call declarative context environment
the body accesses by index (`LoadContextSlot`/`StoreContextSlot`/
`InitContextSlot`/`UpdateContextSlot`) and the closures reach through
the environment walk. The continuation's `this` slots and arguments
slices are done: a non-arrow body referencing `this` gets a frame slot
the certified call fills with the OrdinaryCallBindThis result (methods,
constructors, and class methods now certify; arrows' lexical `this`
stays on the env path); a body's own `arguments` reads get a frame slot
the certified call fills — the unmapped object for strict bodies, and
(the latest slice) the **mapped** object for sloppy simple-param bodies,
aliasing the formals through the capture context (`arguments[i]` reads
and writes the same bindings the body's `LoadContextSlot`s use). The
per-iteration loop-head contexts slice has landed too: a body whose
closure captures a lexical `for`-head binding now certifies — the loop
runs a fresh per-iteration environment per iteration (the existing
`PerIteration` copy machinery, with a new `EnterPerIteration` creating
the first env on the env stack), the head inits write the capture
context, the body's own reads/writes of a captured head go through
per-iteration steps, and the closures capture the fresh env. A closure
capturing a lexical binding *declared inside* a loop body still bails
(its per-iteration block scope is not flattenable into the capture
context). Nested context chains have landed: a certified closure's
references to an enclosing certified body's captured bindings compile
to static `LoadContextSlot { depth ≥ 1 }` reads instead of env walks —
the enclosing bodies' capture-context layouts thread through the
closure-creation steps (innermost first, per-iteration head names open
at the creation site excluded), and the runtime walk skips the
per-iteration envs and named-function-expression self-binding scopes
(the only non-context hops on a certified chain). Deferred from the
continuation: Annex B, the body accumulator model, slot-arg calls, and
the Cut 5 encoding work remain open.

---

## 1. Why the current VM is slow (measured)

- Arithmetic bench ~1.19s / 1M iterations. An empty 1M-iteration `var` loop
  alone is ~630ms. The body's ~5 identifier resolutions per iteration are
  most of the rest.
- Every identifier op (`LoadIdent`, `ResolveVarIdent`, `AssignIdent`,
  `UpdateVarReference`) calls `resolve_binding` at runtime: an env-chain
  walk (`has_binding` per `EnvRecord`) with interner round-trips. V8 does
  this **once, at compile time**: a binding resolves to a register
  (`VariableLocation::PARAMETER`/`STACK_LOCAL`), a context slot
  (`CONTEXT`), or a runtime lookup (`LOOKUP`). That classification is the
  whole difference between parity and fast.

## 2. The V8 reference, pinned to exact sources

- **Variable classification**: `src/ast/variables.h` (`VariableLocation`,
  `IsStackAllocated`, `IsContextSlot`), `src/ast/scopes.cc`
  (`ResolveVariablesRecursively` binds uses to declarations;
  `AllocateNonParameterLocal` → `AllocateStackSlot`/`AllocateHeapSlot`;
  a variable captured by a closure or visible to a sloppy `eval` gets
  `AllocateHeapSlot`). **Read these before designing the scope pass.**
- **Load/store dispatch**: `src/interpreter/bytecode-generator.cc`
  `BuildVariableLoad` (L4707) — `switch (variable->location())`:
  - `PARAMETER`/`STACK_LOCAL` → `LoadAccumulatorWithRegister(Local(index))`
  - `CONTEXT` → `LoadContextSlot(context_reg, variable, depth)` — the
    context chain depth + slot index are **static**; no name matching.
  - `LOOKUP` → `LoadLookupSlot` (the dynamic walk).
- **Contexts**: `src/contexts.h` — a `Context` is a fixed-slot object
  chained to an outer context; closures hold the context pointer.
- **Dispatch**: `src/interpreter/interpreter.cc` — register file +
  accumulator, `Star` stores.

## 3. Instruction-set delta (Cut 3)

New steps (operand types in braces):

| Step | Operand | Semantics |
|---|---|---|
| `LoadLocal` | `{ slot: u32 }` | push frame[slot]; ReferenceError if uninitialized marker |
| `StoreLocal` | `{ slot: u32 }` | pop → frame[slot] (TDZ check on the old value for `let`/`const` assignment) |
| `InitLocal` | `{ slot: u32 }` | pop → frame[slot] (no TDZ check; first write) |
| `UpdateLocal` | `{ slot: u32, op, prefix }` | read slot, apply `++`/`--`, store, push old (postfix) or new (prefix) |
| `LoadContextSlot` | `{ depth: u8, index: u8 }` | walk the context chain `depth` hops, push slot `index` (continuation) |
| `StoreContextSlot` | `{ depth: u8, index: u8 }` | pop → context[chain-depth][index] (continuation) |
| `InitContextSlot` | `{ depth: u8, index: u8 }` | first write, no check (continuation) |

**Landed (first slice)**: the four `*Local` steps only —
`LoadContextSlot`/`StoreContextSlot`/`InitContextSlot` ship with contexts
in the continuation. The dynamic-only steps below stay exactly as they are.

- `LoadIdent { name }`, `ResolveVarIdent { name }`, `AssignIdent { name }`,
  `UpdateIdent { name }`, `PutVarReference*`, `GetVarReference`,
  `PopVarReference` **stay** but become the *dynamic-only* path: they are
  emitted only for bindings classified `LOOKUP`-equivalent (with/eval), and
  for global reads that still resolve through the global environment.
- `EnterBlock { decls }` / `CatchBind { param, decls }`: when the block's
  bindings are frame slots, emit `InitLocal` per binding (TDZ markers) and
  **skip the env creation entirely**; the `EnvRecord` path remains when the
  block is dynamic.
- `CreateFunction { function }` / `CreateArrow`: unchanged shape, but the
  closure's captured environment is the enclosing function's `Context`
  (or the outer env when the enclosing function has none — see §5).

Temporaries stay on the value stack in Cut 3; only *named bindings* become
slots. That is the 80% win (the bench loops' bindings) without a full
register allocator. The accumulator model (§7) later shrinks the temporaries.

## 4. Frame layout

- The `Vm`'s stack region for the active call is the frame (V8's register
  file). Named bindings occupy **fixed slots unique across the function**
  (V8: registers are function-global; each scope's bindings get distinct
  indices — `Scope::AllocateStackSlot` hands out the next register).
- Layout: params first (positional), then hoisted `var`/function
  declarations (initialized to `undefined` at entry), then block
  `let`/`const`/`using`/catch/loop-head bindings in allocation order, all
  pre-filled with the TDZ marker at entry. `InitLocal` clears it.
- `arguments` (§8) is a slot like any other.
- The value stack for temporaries lives above the slot region; the existing
  stack discipline is unchanged.

**Landed (first slice)**: the frame holds params at slots `0..arity` (in
source order), then `var` bindings in first-declaration order.
`setup_frame` copies the first `arity` arguments and fills the rest
(missing args and hoisted `var`s) with `undefined` — matching
`function_declaration_instantiation` for the certified body shape. Block
`let`/`const`/`using`/catch/loop-head slots and the `arguments` slot are
deferred with contexts.

## 5. Context objects and closure capture

- A binding captured by any closure (or visible to a sloppy `eval`) is a
  **context slot** in its declaring scope. A function whose scope has any
  context slots gets a `Context` at entry (a `Vec<Value>` + outer pointer;
  the existing `EnvRecord` grows a `Context` variant so the dynamic paths
  can name-lookup into it).
- The function's [[Environment]] = that `Context` (else the outer
  environment). `register_function`/`instantiate_arrow` store the body's
  `ScopeInfo` (binding → location) on the function record; `ordinary_call`
  uses it to build the frame/context and skip
  `function_declaration_instantiation` + `new_function_environment`.
- Compiled reads of captured bindings use the static
  `LoadContextSlot { depth, index }`; the context chain depth is known from
  the ScopeInfo (V8 `ContextChainDepth`).

## 6. The scope-analysis algorithm (Cut 3, first slice)

Input: the function's AST + the enclosing scope info. Output: a `ScopeInfo`
(binding → location) per function.

1. **Collect scopes**: function params (incl. defaults/destructuring),
   function-scope `var`/function declarations, each block's
   `let`/`const`/class/function declarations, catch params, `for` head
   bindings, `using`/`await using`.
2. **Bind uses to declarations**: walk the body; every identifier read/write
   resolves to the nearest declaring scope (the parser already gives
   `AtomId` names; this is a pure name→binding map, V8
   `ResolveVariablesRecursively`). A use in a nested function body whose
   declaration is here marks the binding **captured**.
3. **Detect dynamic scopes**: a `with` statement or a direct `eval` call in
   a scope makes that scope and every scope it can see **dynamic**: their
   bindings go to context slots (or the env walk, per the promotion rule in
   V8 — a sloppy `eval` can introduce names, so eval-visible bindings must
   be context-allocated).
4. **Classify + allocate**: uncaptured + not-eval-visible → frame slot;
   captured/eval-visible → context slot (index within its scope's context);
   `with`-object names → dynamic (env walk, existing steps).
5. **Emit**: the compiler's `compile_expr`/`compile_statement` consult the
   ScopeInfo: identifier ops emit the slot steps; blocks emit
   `InitLocal`-only entry; function literals capture the context.

**Fallback contract**: any function whose analysis fails (or whose
`ScopeInfo` mode is dynamic) compiles exactly as today — env records, name
walk, all current steps. The two paths coexist; the sweep is the
regression net for the fallback.

**Landed slice (Cut 3, first slice)**: `analyze_scope` certifies a body
when every param is a simple identifier (no rest/default/destructuring)
and the body contains no closures (function/arrow expressions, function
declarations), no direct `eval` or `with`, no `this`/`arguments`/
`new.target`/`super`, no lexical block declarations (`let`/`const`/class/
`using`), no `try`/`switch`/`for-in`/`for-of`, no destructuring
assignments, no tagged templates or private access, and no
`yield`/`await` (async/generator bodies bail outright). The certified body
gets `CompiledBody.scope = Some(ScopeInfo)`; anything else compiles
through the environment machinery unchanged (`scope: None`). This covers
`f(x) { return x + 1; }` and the bench-loop shape exactly. The full
algorithm above (contexts, closure capture, lexical blocks) is the
continuation.

## 7. Cut 4 — accumulator + register model (what makes loops fast)

Cut 3 removes the body's env walks but leaves the empty-loop cost (~630ms
for ~7-15 steps/iter). Ignition's accumulator cuts that: `Lda*` reads into
the accumulator, `Star` stores to a slot, binary ops take accumulator +
slot/imm operands — no push/pop for the common shapes:

- Fused unary/update: `Inc { slot }` / `Dec { slot }` (the arithmetic
  loop's `i++` — currently `ResolveVarIdent`+`GetVarReference`+
  `UpdateVarReference`+`Pop` — becomes one op).
- Fused test-jump: `JumpIfLtImm { slot, imm, target }` (the loop test
  `i < 1_000_000` — currently `LoadLocal`+`BinaryImm`+`JumpIfFalse` —
  becomes one op).
- Arity calls against slots: `CallFast` already reads args off the stack;
  slot-arg calls remove the remaining pushes.

**Measured (first slice landed)**: the fused update and test-jump ops
shipped (`Inc`/`Dec`; the `JumpIfLt/Le/Gt/GeImm` family for `for`/`while`
tests), plus a primitive fast path in the relational evaluator. On a
fast-certified body, the empty loop dropped 0.109s → 0.079s (~28%); the
arithmetic loop stayed flat at ~0.19s while the fusions landed, bound by
`apply_binary`'s machinery — the evaluator numeric fast paths (the
levers below) then moved it to ~0.125s (~25%), with the body's stack
round-trips remaining as the accumulator model's work. The earlier
estimates (~0.6-0.7s after Cut 3, ~0.3-0.4s after Cut 4) were written
against the *top-level* bench, which stays on the env path until the
script-level binding mechanism lands — the fused ops only engage inside
fast-certified bodies, so the gate's arithmetic loop needs top-level
vars first. Slot-arg calls remain open (Cut 4 continuation). Cut 5
(encoding: constant pool, immediates, zero-operand constants,
`--print-bytecode`) closes the gate's second half.

**The step-cost model, measured (fast-function loop, same binary)**: steps
are not uniform — the evaluator-backed steps dominate. An empty loop is
~107ns/iter over 3 evaluator steps; adding `n += 1` (5 mechanical steps)
costs +11ns (~2-5ns/step — nearly free); adding a second `Mul` evaluator
call costs +50ns (~40ns per `apply_binary` call). So the fusions (Cut 4)
and the accumulator (below) remove dispatches that were already nearly
free; the real costs are the evaluator calls (`apply_binary`/
`abstract_relational`/`update_value` at ~20-40ns) and, at top level,
global-object property access (~100-200ns per access — the arithmetic
bench's ~5 accesses/iteration). The evaluator lever has landed: the
number-number check now sits above `to_numeric_operand`/`to_primitive`
for all arithmetic and bitwise ops in `apply_binary` (mirroring `Add`)
and in `abstract_relational` — measured ~25% on the fast-function
arithmetic loop (0.166s → 0.125s, 3-run medians) and ~6% on the
*top-level* arithmetic bench, which stays bound by its global-property
accesses. The remaining lever is a **fast global-property access**
(cell/IC). The gate targets, re-baselined against the documented
2026-08-18 corrected snapshot (perf.md): arithmetic ≤0.50s (5x of 2.52s),
function calls ≤1.15s, property access ≤0.64s.

## 8. The hard corners (risk register — investigate before implementing)

1. **Mapped `arguments`**: non-strict functions with simple params have a
   mapped arguments object aliasing the params. Frame slots break the
   aliasing unless the frame carries a param→slot mapping table the
   arguments object consults. Plan: strict/arrow first (no `arguments`
   binding), then unmapped arguments, then the mapped table.
2. **Annex B block function declarations**: a function declared in a block
   in sloppy mode binds both a block-scoped `let`-like binding and the
   enclosing `var` (with the Annex B hoist). The slot model must allocate
   both.
3. **TDZ**: the marker must round-trip through the value stack (a TDZ read
   in an expression, a TDZ value passed around without being read). The
   marker is a distinct `Value` bit pattern — it must never escape into
   user-visible ops.
4. **eval promotion**: a sloppy `eval` in a scope forces context
   allocation for every binding it can see — *including bindings the eval
   text never references*. Miss this and `eval("x = 1")` silently writes a
   different binding.
5. **Loop-head closures**: `for (let i …) { fns.push(() => i) }` needs a
   fresh context per iteration (the existing `PerIteration` step maps to
   context cloning; an uncaptured `i` stays a frame slot re-init each
   iteration).
6. **Mixed bodies**: a fast function containing a nested dynamic function
   — the nested one's captured environment is the outer `Context`; the
   transition must build the `Context` even though the outer body itself
   only reads frame slots.
7. **`for-in`/`for-of` heads**: `ForBinding::VarDecl` for lexical heads
   creates per-iteration bindings — must allocate slots/contexts, not env
   records, when fast.

## 9. Runtime integration points (exact)

- `crates/runtime/src/ir.rs` — new steps (§3); the Compiler's `compile_expr`
  `Ident`/`Assign`/`Update` arms consult the ScopeInfo; `EnterBlock`/
  `CatchBind` emit slot inits when fast; the `Vm` gains the frame array and
  the `LoadLocal`/`StoreLocal`/`*ContextSlot` handlers.
- `crates/runtime/src/function.rs` — `register_function`/
  `instantiate_arrow` compile the ScopeInfo alongside the body;
  `ordinary_call`/`ordinary_construct` branch on the ScopeInfo mode: fast →
  build frame/context, skip `new_function_environment` +
  `function_declaration_instantiation`; slow → current path unchanged.
- `crates/runtime/src/context.rs` — `resolve_binding` untouched (dynamic
  path); the `EnvRecord` gains the `Context` variant for closure capture.
- The compiler's `scope_stack` (currently control-flow only) gains a
  parallel binding-scope stack for the analysis; `scope_count`-based
  `leave_scopes` still drives the control-flow unwinds.

## 10. Validation per cut

- `cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo
  test --workspace` green, release `--bench` recorded (this machine's runs
  bounce ±15% — report medians, not single runs).
- Full three-area sweep at the known baseline after each cut (rebuild
  `sweep.exe` first — a stale binary silently measures old code; the
  fail+crash union is the comparison, per the `slag-conformance` skill).
- A unit test asserting the fast path is *taken* for the bench-shaped
  snippets (compile `f(x){ return x + 1; }` and check the ScopeInfo/step
  stream contains `LoadLocal` and no `LoadIdent`).
- `--print-bytecode` (Cut 5) becomes the debugging tool for everything
  above; until then the ScopeInfo/step-stream test hook serves.

## 11. Cut order (revised)

1. Cut 1 (done) — compile everything, walker removed, zero regressions.
2. Cut 2 (done, `e0a1ab0`) — `BinaryImm`, `CallFast`, optional-call fix.
3. **Cut 3 — scope analysis + frame slots** (first slice landed — the
   frame-slot half, §13; the continuation — contexts, lexical blocks,
   `arguments`, loop-head bindings — tracks §8 risk items 1-7 and §9).
4. **Cut 4 — accumulator + fused ops** (first slice landed — the fused
   update/test-jump ops and the relational fast path, §7; the accumulator
   model for the body and slot-arg calls remain).
5. **Cut 5 — encoding + `--print-bytecode`** (constant pool, immediates,
   zero-operand constants, the disassembler; closes the gate's first half).
6. **Cut 6 — mapped arguments + Annex B + remaining corners** (§8 items
   1-2) for full fast-path coverage.

The v8 drop-in crate is a separate workstream (an export surface, WIP) and
is not a consumer of this plan; the `v8/` checkout here is the *reference
implementation* for the VM itself.

## 12. Non-goals

- No ICs/feedback (TurboFan tier-up; out of scope).
- No GC rewrite (separate perf.md milestone).
- The regexp engine is a separate workstream.
- No full register allocator for temporaries in Cut 3 — named bindings
  only; the value stack keeps handling expression temporaries until Cut 4.

## 13. Cut 3 landing — first slice shipped (the task doc, folded in)

### What shipped (verified against the diff)

- **`crates/crux/src/value.rs`** — the TDZ marker: `TAG_UNINITIALIZED`
  (tag 9 in the reserved NaN-box range), `Value::uninitialized()`,
  `Value::is_uninitialized()`. It lives only in VM frames; every frame op
  checks it before the value can reach user-visible ops, so it never
  escapes (`kind()`'s reserved-tag `unreachable!` stays unreachable).
- **`crates/runtime/src/ir.rs`** —
  - `ScopeInfo { frame_size, arity, slots: name→slot, tdz_store }` and
    `CompiledBody.scope: Option<ScopeInfo>` (`None` = env path).
  - The four steps from §3: `LoadLocal`/`StoreLocal`/`InitLocal`/
    `UpdateLocal`, with TDZ checks on `Load`/`Store`/`Update`.
  - `Vm.frame: Vec<Value>` + `setup_frame(scope, args)` (§4 layout).
  - `Compiler.scope: Option<ScopeInfo>`; emission changes: identifier
    reads → `LoadLocal`; `var x = init` → `InitLocal`; `x = v` →
    `StoreLocal` (value kept for the expression); `x += v` →
    `LoadLocal; RHS; Binary(compound); StoreLocal`; `x++` → `UpdateLocal`;
    `typeof`/`delete` of a slot read the slot; `for (var i = 0; ...)`
    heads `InitLocal`; blocks with no lexical declarations skip
    `EnterBlock`/`LeaveBlock` entirely (the per-iteration env allocation
    in hot loops disappears).
  - The scope-analysis section at the bottom of the file (`analyze_scope`
    + the statement/expression scanners with the bail conditions of §6),
    wired into `compile_body` (`scope: compiler.scope`);
    `compile_statements` (top level) stays `scope: None`.
- **`crates/runtime/src/function.rs`** — `ordinary_call` takes the fast
  branch when `ir.scope.is_some()`: push the execution context with the
  closure's captured env (no `new_function_environment`, no
  `function_declaration_instantiation`, no `this` binding — the certified
  body references none of them), run the body with the args, pop. The
  slow path is untouched. `run_compiled_body` gains the args and calls
  `vm.setup_frame(scope, args)` before `start`; `ordinary_construct`
  sets up the frame the same way (a constructed fast function's `this`
  isn't referenced, so the frame path is identical; kept for later when
  `this` slots land).
- **`crates/runtime/src/expr.rs`** — `compound_binary` made `pub(crate)`
  for the compound-assign slot path.
- **`crates/runtime/src/eval.rs`** — fast-path unit tests: the step
  stream of `f(x) { return x + 1; }` and the bench-loop shape contains
  `LoadLocal`/`InitLocal`/`UpdateLocal` and no `LoadIdent`/`EnterBlock`;
  behavior tests (`f(41) === 42`, var hoisting, TDZ-free var semantics,
  assignment-expression value, `typeof`/`delete` of slots, extra call
  arguments, for-head init, outer-binding reads through the env) and
  slow-path regressions (`this`, `arguments` still behave).

### Validation results

1. `cargo build --workspace` + `cargo clippy --workspace --all-targets
   -- -D warnings` clean.
2. `cargo test --workspace` green — runtime 565 passed (15 of them the
   new fast-path tests), the test262 harness 3322 passed / 2 ignored,
   matching the clean baseline exactly.
3. `--bench` A/B against the clean baseline: **function calls ~2.2x
   faster** (2.00s vs 4.48s medians — the `f(n)` body's `x` read is a
   frame slot); arithmetic/property/concat flat (top-level stays
   `scope: None`, as documented); array iteration within noise.
4. The three-area release sweep vs the clean baseline: the fail+crash
   unions are strict subsets everywhere (language 152 ⊆ 155, built-ins
   112 ⊆ 113, annexB 1 = 1); four baseline failures now pass. The only
   hang wobble is the pre-existing, load-dependent
   `RegExp/property-escapes` cluster (totals 239 vs 244, crash counts
   identical).

### Known limitations of the slice (documented, not bugs)

- Top-level `var` bindings are not slots at this slice (top level stays
  `scope: None`), so the arithmetic/property/concat benches don't move yet
  — that's a separate mechanism (script-level bindings), the follow-on to
  Cut 3, **which has since landed** (`docs/perf.md` Cuts 5-16: fast
  scripts + script var slots closed the gate).
- `arguments`, `this`, closures **that capture a body binding**, mapped
  arguments, Annex B — all bail to the env path; correctness preserved,
  speed later (the Cut 3 continuation + §8 risk register). A certified
  body may create capture-free closures (the continuation's first slice).
  **This list has since shrunk**: `this` slots, unmapped strict
  `arguments`, mapped sloppy `arguments`, capture-based closures,
  per-iteration loop-head contexts (a closure capturing a `for`-head
  `let` runs the loop through per-iteration envs), and nested context
  chains (a certified closure's references to enclosing certified
  bodies' captured bindings compile to static context-chain reads)
  landed in later slices (`docs/perf.md` Cuts 17+); still deferred are
  loop-body-scoped captures (a closure capturing a lexical binding
  declared inside a loop body bails — its per-iteration block scope is
  not flattenable), and Annex B.

### Correctness notes (why the slice is right)

The frame path pins the edge cases the conformance net exposed:
assignment expressions keep their value on the stack (a plain
`StoreLocal` alone would drop it — the harness's `while ((x = f()) !==
expected)` shape depends on it); `typeof`/`delete` of a slot read the
slot (the env walk finds no binding in a fast body); `for (var i = 0;
...)` heads initialize the slot (`Destructure` would walk the env for a
binding the fast call path never created); and `setup_frame` copies only
`arity` arguments so extra call arguments never land in `var` slots.

## 14. The Atomics hang investigation (resolved)

The Cut 3 validation surfaced one hard-to-read failure: the test262
harness's `debug_atomics_fixture` (`Atomics/notify/notify-one.js`)
hung (>60s) only with the working-tree changes; clean HEAD passed in
~1.2s. The trail is preserved here because it is easy to misread:

- **Evidence**: the fixture spawns 3 worker agents (`$262.agent.start`
  → OS threads), then `safeBroadcast`, `waitUntil(RUNNING, 3)`,
  `notify(0, 1)`, `trySleep(TIMEOUT)`, and 3 `getReport()`s. The
  JS-side `waitUntil`/`getReport` loops have no deadline, so a hang is
  indistinguishable at a distance from a worker that never reports.
- **The red herring**: a temporary `CUT3-FAST` probe printed only for
  certified bodies and printed nothing before the timeout, suggesting
  the fast path was uninvolved. That conclusion was wrong — the
  probe's stderr was not captured cleanly, and the harness's *own*
  helper functions (`$262.agent.waitUntil`, the `getReport` override,
  `$DETACHBUFFER`, `$262.detachArrayBuffer`) are simple-param
  functions and are certified fast. The fixture's receiver callbacks
  do bail (`const`), which is what the probe reading was mistaken for.
- **Root cause**: the fast path's plain-assignment arm emitted
  `StoreLocal` without a `Dup`, dropping the assignment *expression*
  value. `waitUntil`'s `while ((agents = Atomics.load(...)) !==
  expected)` test then compared `undefined !== expected` — true
  forever — so the main thread spins in `waitUntil` and the fixture
  never completes (the workers time out and leave; the main thread
  never collects).
- **Fix + confirmation**: `Dup; StoreLocal` restores the expression
  value; the fixture passes again (1.16s vs clean HEAD's 1.2s). The
  same class of bug (a fast-path arm consulting the environment for a
  slot binding) was then caught by the vendored harness cluster before
  the sweep: `typeof`/`delete` of slot names resolved via the env
  (breaking `$DETACHBUFFER` and the JSON BigInt replacer — 14
  fixtures in `cargo test -p test262 --lib`), the `for (var i = 0;
  ...)` head using `Destructure`, and `setup_frame` copying args by
  frame size. All are pinned by the §13 correctness notes and unit
  tests.
- **What was ruled out**: the `Vm` struct change (the `frame` field)
  and `run_compiled_body`'s new signature do not affect the atomics
  worker/resume machinery — worker scripts are top level (stays
  `scope: None`) and the fast harness functions run on the ordinary
  call path.
