---
name: slag-bytecode-vm
description: "Load when working on Slag's bytecode VM and compiler (crates/runtime/src/ir.rs) — the Step enum, the Vm dispatch loop, compile_expr/compile_statement lowering, the LeafOp register executor, the certified-function frame-slot gate, or the compiled-path stack protocol. Documents the non-obvious traps: optional-call short-path stack discipline, assignment-reference timing, super base capture before key conversion, destructure/for-of close semantics, template cache keys, the leaf-inline caller-Vm contract, the register-leaf lowering/aliasing contracts, the fused global-call ordering tradeoff, the frame-slot certification gate, and the bench-noise reality."
---

# Slag bytecode VM traps

The engine compiles every expression and statement to `Step` bytecode
(`crates/runtime/src/ir.rs`) and runs ordinary calls/constructs, generators,
async functions, and top-level scripts on the `Vm` dispatch loop. The old
tree-walker survives only as isolated single-expression helpers (computed
keys, destructuring defaults, class heritage) — never route statement or
control-flow structure back through it. These are the traps that cost real
debugging time; the vendored unit suite stayed green throughout, so the
full conformance sweep is the regression test for this file.

## The v8 reference checkout is gitignored

The proper reference for the VM work is the vendored V8 checkout in
`v8/` (`src/interpreter/bytecodes.h`, `bytecode-generator.cc`
`BuildVariableLoad`, `src/ast/scopes.cc` `AllocateNonParameterLocal`,
`src/contexts.h`). It is in the repo's local `.gitignore`, so the agent
file tools (`read_file`/`grep`/`find_path`) treat it as absent — read it
via the **terminal** (shell grep/sed/`ls`) instead. Never conclude from
an empty agent-tool result that the checkout is missing.

## 1. Optional-call short paths must pop the receiver too

`compile_optional_call_tail`'s nullish path runs with `[receiver, callee,
callee-dup]` on the stack — the `Dup`'d callee was pushed back by
`JumpIfNullishKeep`. Popping two leaves the receiver on the stack, and
every later stack read in the enclosing expression shifts (a leaked
`this`/object lands where the next operand, callee, or arg is expected).
Pop three, then push `undefined`. All four callers (plain `f?.(x)`,
`obj.m?.(x)` with and without `member.optional`, `super.m?.(x)`) have the
same 3-value shape at the short label. Regression tests live in
`eval.rs::tests::optional_call_nullish_callee_short_circuits`.

## 2. Assignment targets resolve their reference before the RHS

`ResolveVarIdent` + `GetVarReference` run BEFORE the RHS evaluates, and the
reference is kept on the `var_ref_stack` until `PutVarReference` /
`PutVarReferenceOp` / `UpdateVarReference`. PutValue must use the initially
created reference even when the binding disappears while the RHS evaluates
(spec 13.15.3 — a `with` scope whose binding the RHS deletes). The
short-circuit `&&=`/`||=`/`??=` paths drop the reference with
`PopVarReference`.

## 3. Super reads/writes capture the base before the key evaluates

`GetSuperBase` runs the this-binding check and captures the base BEFORE the
computed key's expression evaluates — a key whose `toString` mutates the
prototype must see the original base. The compound/update paths
(`GetMemberComputedKeep` / `GetSuperComputedKeep` + `Dup2`) convert the key
once and leave the CONVERTED key on the stack for the write (spec 13.15.4:
ToPropertyKey runs once, before the read).

## 4. Destructure/for-of close ordering

- `DestructureClose` pops its iterator before closing — a throwing `return`
  method must not re-close the iterator when the error unwinds through
  `run_inner`'s close-from-error path.
- The `for_of_stepping` / `destructure_stepping` flags must stay set on the
  error path: they guard `run_inner`'s close-from-error, so a `next()`
  error must NOT close the iterator (spec: IteratorClose only on normal
  completion).
- `restore_per_iteration` is emitted ONLY for lexical `for` heads — an
  unconditional env unwind broke `var`/expr-head loops.

## 5. Template cache keys are not parse-node pointers

The template-object cache key is `(generation, realm, span.start,
span.end)`, NOT the AST node pointer — the compiled path embeds a clone of
the quasi per compilation, so a node pointer changes identity between
compilations of the same source text.

## 6. CallFast stack layout and the vector-form escape hatch

`CallFast { argc, direct_eval }` expects `[this, callee, arg1..argN]` on
the value stack and reads the argument slice in place (no args-vector
build, no `Vec` alloc). `compile_arguments(args, allow_fast)` returns
`Some(argc)` for 0-2 plain arguments, `None` for the vector form —
`allow_fast` is false for `Construct` and `SuperCall`, whose VM handlers
still pop from `args_base_stack`. `BinaryImm { op, imm }` is emitted only
when the RHS is a `Number` literal (a `Str`/`Null`/`Boolean` RHS goes
through coercions and must stay a generic `Binary`).

## 7. Leaf-inline runs on the CALLER'S Vm (Cut 25)

`CompiledBody::leaf` marks a certified body whose steps let a call to it
run in place on the caller's Vm (`do_call_fast` / `Step::Construct` →
`run_leaf_body`): no execution-context push, no pool round-trip, no frame
re-alloc (~82ns/call). Three contracts keep it correct; break any of them
and you get wrong behavior only when an uncertified caller with try/with/
for-of/destructure state calls a leaf:

- **`steps_are_leaf` must exclude every step that reads the running
  execution context or writes a shared VM stack.** An inlined body sees
  the CALLER'S env (`CreateFunction`/`CreateArrow` capture
  `running_context()?.lexical_environment` — a leaf closure would capture
  the caller's bindings), so all reference machinery (`LoadIdent`,
  `Resolve*Ref*`/`GetVarReference`/`Put*VarReference`), `NewTarget`,
  super/private steps, env-machinery steps, and sloppy mapped
  `CreateArguments` (reads the context's `function`; strict unmapped is
  fine — it reads only `call_args`) are out. When you ADD a new Step,
  decide its leaf safety explicitly — the default for anything touching
  `agent.running_context()` or `try_stack`/`pending`/`for_of_*`/
  `for_in_stack`/`destructure_*`/`env_stack` is exclusion.
- **`can_inline_leaf` guards the call site, not the body**: the caller's
  try/pending/for-of/for-in/destructure stacks must be empty and
  `env_stack.len() == 1`. This is what makes the leaf's own
  `Return`/`Throw`/`Break`/`Continue` safe — they route through
  `control_transfer`/`throw_machinery`, which walk the SHARED stacks with
  the LEAF's (empty) handler table; with caller frames present,
  `find_finally_frame`'s `?` short-circuits and the close helpers would
  close the caller's iterators.
- **Run via `run_inner_inner`, never `run_inner`**: a leaf error must
  propagate raw to the caller's `run_inner`, which applies handler
  coverage, iterator close, and disposal with the caller's body and the
  restored `ip`. `run_inner`'s own error machinery would run those
  against the leaf's empty stacks/tables and close iterators that the
  caller's try was about to catch.

The inline save/restores every field the leaf can touch (`ip`, frame swap,
stack/list/completion/var_ref/array_index stack lengths, `completion`,
`acc`, `strict`, `body_context`, `chain_short`, `call_args`) on BOTH paths;
`self.global` needs no save because the path is gated on a single realm
(cross-realm dispatches fall through to `call_inner`). Construct inlining
needs the certified-construct conditions PLUS `is_method`: `{ method() {} }
is a certified leaf whose `new` must throw "not a constructor" — the
language-area sweep fixture
`expressions/object/method-definition/name-invoke-ctor.js` caught exactly
this. Edge-case matrix: `scratch/leaf_inline_probes.js` (gitignored).

## 8. Register leaf bodies (Cut 35 slice 1)

A leaf body whose steps all lower to `LeafOp`s (`CompiledBody::leaf_ops`
is `Some`) runs on a dedicated register executor (`run_leaf_regs` /
`run_leaf_ops`) instead of the step dispatch loop. The register model is a
single accumulator (`Vm.acc`) plus the leaf's frame segment, capture
context, and (for per-iteration reads) lexical env — no value-stack
push/pop, no `ip`, no completion, no `strict`, no `chain_short`/array-index/
arguments state.

- **`lower_leaf_ops` accepts only left-leaning straight-line shapes**: a
  binary's left operand must be the accumulated value and its right a
  directly-addressable operand (frame slot, depth-0 context slot, depth-0
  per-iteration slot, or const). A right-leaning `a + (b + c)` temp, a
  `Dup`/`Pop`, a `return` followed by dead code, any jump/branch, any
  depth ≥ 1 context read, or a fall-off body → `None` (step path). This is
  a strict subset — when in doubt, a body stays on the step path.
- **`LoadContext`/`BinContext` must skip context-transparent envs**: the
  step path's `context_chain_env(0)` walks out past a named function
  expression's self-binding scope (holds the function itself at slot 0)
  and per-iteration copies before reading the capture context. Missing
  this walk, a `return (x) => x + a` inside `function mid() {...}` reads
  `mid` instead of `a` (the self-binding env's slot 0).
- **The frame can alias the caller's argument region**: `LeafFrame::Alias`
  when `this_slot.is_none()`, `frame_size == arity` (every slot a param),
  and `argc >= frame_size` — no argument copy, no frame push, and the
  post-run truncate is skipped (the caller discards the aliased slots).
  `LeafFrame::Pushed` covers the rest (missing args → `undefined`, `this`
  slot, var/TDZ slots). A register body's `StoreReg` may write the aliased
  slots — the writes are discarded by the caller's truncate.
- **`do_call_fast` runs register leaves directly**: no
  `run_leaf_call`/`run_leaf_body` indirection and no completion
  round-trip (a register body always completes `Return`); the
  `OrdinaryCallBindThis` logic is the shared `bind_this_value` helper. The
  construct path (`run_leaf_construct`) still routes through
  `run_leaf_body` → `run_leaf_regs` with `LeafFrame::Pushed` (it needs the
  completion for the base-constructor object-return rule).
- **A leaf's capture context is always empty** (leaves create no closures,
  so nothing is captured into them), so `new_body_context` always returns
  `None` for a register body and the env setup never uses the arguments —
  which is what makes the aliased path safe passing no argument slice.
- **`StoreMemberName` rejects an `Acc` value** (Cut 35 slice 6): a
  computed-value store like `this.y = a + b` computes the value into the
  accumulator, and the object load would then overwrite it — the same
  restriction as the binary ops' right operand. The `Function/S15.3.5_A3_T2`
  fixture (`new Function("arg1,arg2", "...; this.y=arg1+arg2;...")`)
  wrote the object into `y` until the rejection was added. `SetCompletion`
  is skipped by the lowering (the register path's `Empty` maps identically
  to the step path's `Normal` for leaf callers), and a body ending in a
  store — or empty — now lowers (fall-off completes `Empty`).
- **`StoreMemberComputed` shares the step path's machinery** (Cut 35 slice
  7): `o[k] = v` lowers with the key + value as operands (neither may be
  `Acc` — the object load clobbers), and both the op and the
  `AssignMemberComputed` step's plain `=` branch call the extracted
  `assign_computed_plain` (nullish check, fast array element write,
  `to_property_key` + `assign_member`) — extend the helper, not the two
  call sites separately.
- **`GetMemberName`/`GetMemberComputed` reads** (Cut 35 slice 8): `o.x` /
  `o[k]` in value position lower with the object in the accumulator (the
  computed key a direct operand, never `Acc`), sharing the step path's
  `get_member_name`/`get_member_computed` helpers (nullish check,
  member-cell cache, fast array element read, property-key conversion).
  Reads compose with the other register ops (`return o.x + 1` lowers),
  and getters run through the same agent-side machinery on this Vm.
- **`RunRegBody` runs register-lowered LOOP bodies** (Cut 35 slice 9): the
  certified canonical `for` and for-of/in bodies lower via
  `lower_leaf_ops` (the value-free `ListBegin`/`ListEnd`/reset/normalize
  wrappers are skipped) and run in one dispatch against the current
  frame. The register ops address slots via `frame_get`/`frame_set` — the
  leaf path resolves through `leaf_frame_base` (the stack segment), the
  script path through the inline `Frame` — so the same ops serve both. A
  body with a jump/`PushAcc`/two-read shape stays on the step path; the
  accumulator-loop counter survives via the step's save/restore.
- **Fused member reads + spills** (Cut 35 slice 10): `GetMemberNameLocal`/
  `GetMemberComputedLocal` fuse a frame-slot object into the read (one
  dispatch, `tdz` carried); `PushAcc` spills the live accumulator onto the
  value stack when an op must overwrite it; `BinAccPop` pops it back as
  the binary's left operand; `BinLeftReg` computes `frame[slot] op acc`
  for a combine whose left is a frame slot and whose right is the
  accumulator's live value. The shadow stack gained `RegOperand::Spilled`
  (consumed only by `BinAccPop`; any other load of a spilled operand
  rejects the body). `n += o.a + o.b` now lowers to a six-op body in one
  dispatch.
- **`BinLeftReg` reads the frame slot at the combine — after the
  accumulator value was computed** (Cut 35 slice 10). That is safe ONLY
  for `tdz=false` slots: a member read's getters cannot reach the body's
  own frame slots (they run in other functions with no handle on this
  frame), and the accepted shapes write no slot between the load and the
  combine. The lowering rejects `tdz=true` slots (the step path must
  throw the TDZ ReferenceError before the reads run). The op preserves
  operand order (`n + sum`, never `sum + n`) — the string-concat probe
  catches a swap. A captured/constant left with a computed right (`x +
  o.a` with `x` captured) still cannot lower: the capture context is
  reachable by other closures, so its slot can be mutated by a getter.
  The spill is only emitted when the value below the popped object is a
  live `Acc` — a computed `Acc` key is rejected in the fused computed
  read because the spill would push the key, not the live value.
- **The member value cache is generation-validated** (Cut 35 slice 11):
  `member_cell_get` fronts its slot cache with (object id, name,
  generation, value) — a generation match returns the value with no
  property-vector borrow. This is sound ONLY because every own-property
  mutation of an Ordinary/Array object bumps the generation, including
  the in-place paths that previously missed it: `set_key`'s in-place
  write, `array_element_write`'s element write and dense append, and
  `array_define_own_property`'s dense append (`length` update). If you
  add a new path that mutates a property-vector entry in place (a
  `*slot = value` on `props`), it MUST bump — a stale cached value would
  be returned otherwise. The extra bumps cost the write-side chain cache
  and for-of caches spurious misses (re-validated, correct), never stale
  `GetMemberNameLocal` reads its frame slot by reference and
  tries `member_cell_get` before cloning the value for the full fallback
  (one fewer refcount bump per hit).
- **Call-site leaf caches** (Cut 35 slice 12): `fast_call_core`'s
  leaf-run block is extracted into `run_inline_leaf` (takes the
  `LeafEntry` by value), and the resolved entry is cached per call site
  on the first call. The global cache (`global_leaf_cells`, name →
  entry) is validated by the global object's identity + generation —
  sound only because slice 11 made every global mutation bump. The slot
  cache (`slot_leaf_cells`, frame-slot index → entry) is validated by
  the callee's `heap_payload` (the raw leaked-Rc pointer): the cache
  holds the callee Value so the closure's allocation can never be
  recycled, making a payload match exact — a reassigned slot misses and
  re-resolves. Both cache-hit paths must re-check `can_inline_leaf` and
  `realms` per call (they are per-call-site state, not callee
  properties). `Value::heap_payload` is `None` for doubles.
- **The array element and length reads are generation-validated** (Cut 35
  slice 13): `array_element_get` fronts its slot cache with (id, index,
  generation, value) and `array_length` with (id, generation, length) —
  a generation match skips the property-vector borrow and validation.
  Sound for the same reason as the member value cache: slice 11 made
  every array mutation (in-place element writes via `set_key`/
  `array_element_write`, dense appends, length updates, defines,
  deletes) bump the generation. The for-of fast path re-reads the
  length and each element every step (stock iterator semantics), so
  these caches are what make the array-iteration row fast.
- **Leaf binary fusions** (Cut 35 slice 14): `BinImmLocal` fuses
  `LoadReg`+`BinImm` (a frame-slot left with an immediate — `return x
  + 1`) and `BinCtxReg` fuses `LoadContext`+`BinReg` (a captured left
  with a frame-slot right — `(y) => x + y`), each one dispatch with the
  `tdz`/env-walk semantics of the parts preserved. Safe because the
  two reads (env + frame) have no side effects between them, so the
  step path's evaluation order is unchanged. The `Binary` arm's `else`
  is a `match (left, right)` with the fused case first — add further
  fused source pairs there (e.g. `Reg`+`Reg`) rather than emitting
  `load_operand` + `Bin*`.
- **The `Construct` step shares the leaf cache** (Cut 35 slice 15): the
  certified construct-inline verdict (`construct_inline` — leaf body,
  base kind, no fields/private methods) rides on `LeafEntry`, and the
  `Construct` handler reads `leaf_lookup` instead of the
  `ecma_functions` HashMap. A construct-inline body is always a leaf,
  so the leaf cache's filter is correct; arrows/classes/derived
  constructors are not `construct_inline` and fall to the general
  `fast_call_core`. If you extend `LeafEntry`, update both the agent
  `leaf_lookup` population and the slice-12 cache-write clone in
  `fast_call_core`.
- **The accumulator-loop counter read lowers too** (Cut 35 slice 16):
  `RunRegBody` gained `push_counter` (the saved counter is pushed at
  entry) and `LeafOp::LoadCounter` pops it; `Step::PushAcc` lowers to a
  `RegOperand::Counter` shadow entry consumed by `load_operand`. At most
  one counter read per body (the counter is pushed once), and a counter
  in a store key/value or binary right position is rejected (the
  operand loader cannot pop there). The two `RunRegBody` emission sites
  scan the body slice for `Step::PushAcc` to set the flag — if you add a
  third site, mirror that.
- **The slot-callee call-store fusion** (Cut 35 slice 17):
  `emit_statement_store` recognizes the tail pattern `LoadLocal(args),
  CallFastSlot, FusedStoreLocal` and replaces it with
  `CallFastSlotStore` (arg slots read in order with `LoadLocal` TDZ
  checks, the slot-callee call via `do_call_fast_slot`, the result
  stored with the `FusedStoreLocal` TDZ check). The pattern only fires
  on a real `CallFastSlot` emission (its guards already passed) with
  plain slot args — member/nested/compound/expression-position shapes
  keep the step path. `CallFastSlotStore` is excluded from
  `steps_are_leaf` like the other call steps.
- Errors propagate raw to the caller's `run_inner` exactly like a
  step-path leaf (a register body may throw from `apply_binary`/TDZ
  checks). Register-op semantics mirror the step semantics 1:1 — when you
  extend `LeafOp`, extend both `lower_leaf_ops` and `run_leaf_ops` and
  keep the mirror exact.

## 9. Fused global calls reorder GetValue after the arguments (Cut 35 slice 2)

`CallFastGlobal` (compiler branch in `compile_call`, handler
`do_call_fast_global`) fuses a plain call to a declared top-level
`var`/function global into one step: the receiver push and the callee load
vanish, the handler reads the global cell (`load_global_value`) and passes
`undefined` as `this`. The compile-time guard is narrow — no `?.`, no
`with`, no `eval` identifier, `chain_depth == 0`, ≤ 2 plain args, and
`binding(name) == BindingLoc::Global` (only certified top-level scripts
carry `script_globals`, so the fuse never fires inside function bodies).

- **The callee GetValue runs AFTER the arguments** (spec 13.4.3 evaluates
  the callee in step 2, the args in step 4): the fused handler reads the
  global cell only once the args are on the stack. This is unobservable
  for a declared var's data property; the divergence needs a declared
  global redefined as an accessor with side effects, which the
  certified-script model (direct-mapped `global_cells` cache) treats as
  out of scope. If you extend the fused shape, keep this tradeoff in
  mind — a fixture could someday catch it.
- **The global fuse requires the callee NEVER-ASSIGNED and no call-like
  args** (the slice-2 probe caught `f(f = g)` calling the NEW `f` on the
  global-only path): the compiler checks the script's assigned set
  (prepass, now walking function bodies so an uncertified function's
  write to the name counts) and that no argument contains a
  `Call`/`New`/tagged template (a builtin like
  `Object.defineProperty(globalThis, ...)` could rewrite the global
  callee). `CallFastSlot` (a frame-slot callee — a certified-value var)
  needs only the direct-arg-write check (`expr_assigns_name`): a
  certified script's args can write a declared var only directly, and a
  slot read is side-effect-free.
- **`compile_call_args_guarded` skips the guard at `chain_depth == 0`**:
  `chain_short` is set only inside a chain and cleared by the outermost
  chain node's `ClearChainShort`, so the `JumpIfChainShort`/`Jump` pair is
  dead outside a chain. The skip also covers paren'd chain callees
  (`(a?.b)()`, `(a?.(x))(y)`): the chain ends at the paren, so the call
  must RUN on the chain's value (throwing on `undefined`) — a stale guard
  there would wrongly skip it.

## 10. The frame-slot gate certifies global-blind callables (Cut 35 slice 3)

A slotified script (Cut 16: declared vars live in frame slots, the loop
counter in the accumulator, epilogue write-back) is a stale-global window:
while the slots are authoritative, the global object holds the initial
values, so ANY callable that can observe the global object (read/write a
declared var, touch `globalThis`, run `eval`/`with`, escape a closure)
forces the whole script back onto the global path. Slice 3 certifies
callables that provably cannot observe it:

- `certified_functions` (fixpoint over the top-level function
declarations): a body may read/write only its params, locals, undeclared
names (real global properties — never stale), and the stable
entry-instantiated bindings of other certified functions (whose names are
never assigned); it may create certified closures (function/arrow
expressions with global-blind bodies) and call certified functions, but
no `this`/`super`/`eval`/`with`/`try`/`switch`/`for-in`/`using`/
`globalThis`-family names, no uncertified calls. Recursion never
certifies; an assigned name is never a candidate.
- `certified_value_expr` + `collect_var_certs` extend the top-level call
gate to vars holding a certified value: a certified closure, a literal,
or a certified-call result (the callee's return is itself certified —
`certified_return_certs` fixpoint over the declarations' `return`s).
Multiple assignments AND; compound/`++`/`--` marks the var unknown.
- **Certified functions stay GLOBAL bindings** — never slotted: their
  stable entry-instantiated function objects sit on the global object
  (`global_declaration_instantiation` hoists them), so `binding()` returns
  `Global`, top-level calls fuse (`CallFastGlobal`), and the `FunctionDecl`
  statement compiles to nothing on the frame path.
- **The gate is all-or-nothing per script** and the analyses are purely
  syntactic (they run in `analyze_script_scope` before compilation, with
the assigned set collected by a prepass). When you extend it, the danger
is a WRONG CERTIFICATION (a stale-global read inside a certified body) —
the 15-case probe `scratch/certified_fns_probe.js` has the regression
cases; the full sweep is the backstop.

## 11. The certified for-of/for-in do-while back edge (Cut 35 slice 18)

The certified loop was `[Fetch; body; Jump(top)]` — the back-jump was one
of the three per-element dispatches. The protocol steps
(`ForInNext`/`ForOfNext`/`ForOfNextBindLocal`) now carry `back`, and the
loop shape is a do-while: the prologue fetch at `top`, the body, then a
**duplicate fetch at the loop bottom that IS the back edge** (its `back`
targets the head bind / body start above). A straight-line body has no
per-element jump dispatch (the array row drops ~2-8ms).

- **`back` = the step right after the PROLOGUE fetch** (`step_index + 1`):
  the head bind when not fused, the body start when fused. Pointing it at
the body start for a NON-fused head leaves every element on the value
stack (the bind never runs) and grows the stack per iteration.
- **The `continue` target is the per-iteration copy (captured heads) or
the loop-bottom fetch itself** — the copy must run between the body and
the fetch, exactly the old `Jump(top)` ordering. A `continue` falls into
the copy/fetch and re-runs it.
- **`resolve`'s `Fixup::ForInNext`/`Fixup::ForOfNext` must pattern-match
`{ done, .. }`** to preserve the compiler-patched `back` — reconstructing
the variant would reset it to 0. `ForOfNextBindLocal` already did this.
- **The generic (uncertified) paths keep the `Jump`**; their `back` is
just the next step (fall-through equivalent, no behavior change) because
their back-edge restore steps (`ForOfRestore`/`ForInRestore`) must still
run per iteration.
- **The `ForOfBoundary` span `[top, end]` is unchanged** — the loop-bottom
fetch sits inside it, so `close_for_of_upto` (external transfers) behaves
identically.
- A body that lowers to `RunRegBody` can't contain a continue (register
ops reject jumps), so the do-while applies there unconditionally; the
`RunRegBody` truncation keeps the prologue fetch (it precedes
`body_steps`), so the patched `back` stays valid.

## Bench reality (Cut 2)

`cargo run -p cli --release -- --bench` bounces ±15% on this machine (the
array-iteration benchmark has ranged 13.7–19.1s across identical runs) —
only judge consistent multi-run deltas, never a single run. An empty
1M-iteration `var` loop alone is ~630ms of the ~1.19s arithmetic bench: the
loop machinery (dispatch + env resolution) dominates, so encoding fusion
moves numbers within noise, and the perf.md ≥5x gate needs structural work
(registers/accumulator, i.e. the plan's Cut 3), not more step fusion.

**The direct-mapped caches thrash with many keys — a realm-wide
measurement artifact (Cut 35 slice 5).** The bench evaluates each row
twice in ONE realm, so by the construct row ~30+ globals exist and the
32-entry `global_cells` cache (Cut 5) collides: the construct row's
`C`/`i`/`n`/`o` took the reference path on every access and the row
measured ~74ms vs ~36ms standalone for the identical loop. A bigger
`GLOBAL_CELLS` (256) fixed it; a row that measures slow only inside the
bench, but fast standalone, is a cache-collision artifact — check the
cache sizes before optimizing the machinery. The other direct-mapped
tables (`MEMBER_CELLS`, `LEAF_CACHE`, the for-of/element cells) are 16
entries and have the same failure mode at scale.

**Step fusion has hit the floor (Cut 35 slices 19-20, measured 2026-08-23).**
The dispatch is a jump table: removing a dispatch per iteration measures
ZERO (fusing the `FastLoopHead` into the register body — slice 19,
reverted — and fusing the global `CallFastGlobal`+store into one step —
slice 20 — both measured ~0ms on 5M-iteration A/B runs). The remaining
per-iteration costs are REAL WORK: the loop head's inc+test is ~11ns/iter
(the empty 1M `var` loop is ~12ms regardless of the dispatch count), and
the leaf-call core (cache check + frame setup + `run_leaf_ops` + truncate)
is ~20ns. Don't propose another step-fusion slice expecting a win; the
levers left are structural (a dedicated counter field removing the acc
save/restore, or a call ABI reading args straight from frame slots).

**A/B bench methodology: alternate the order.** The machine drifts within
seconds (a base-then-new pair can show a consistent +2-5ms "regression"
that is pure order bias — this mis-attributed slice 19's zero to code
bloat). Interleave base/new per pair and cancel the order; use a
5M-iteration isolated timer (not the full bench) to amplify the signal
above the load noise.

## Validation loop

`cargo clippy --workspace --all-targets -- -D warnings` clean, then
`cargo test --workspace` green — then REBUILD the sweep
(`cargo build --release -p test262`) and run the full areas; the compiled
path is shared, so a local fix can regress anywhere. For an A/B regression
check, diff the fail+crash union against the parent worktree at
`C:/Users/T/Desktop/jsrt-parent` (at `8c2f0cf`; its sweep binary must be
rebuilt from that source). See the `slag-conformance` skill for the sweep
workflow and the load-dependent classification trap.
