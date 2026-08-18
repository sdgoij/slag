# Bytecode for everything: the runtime IR, Ignition-style

Status: plan (no code yet). Goal: remove the tree walker from normal
execution by compiling every expression and statement to `Step` bytecode,
mirroring V8's Ignition architecture. The regexp engine is a separate,
smaller follow-on (see the end); this document is about the runtime.

The reference is the V8 checkout in `v8/`:
`src/interpreter/bytecodes.h` (instruction list), `bytecode-generator.cc`
(AST lowering), `bytecode-register-allocator.h` + `bytecode-register-optimizer.cc`
(register allocation), `interpreter.cc` (dispatch).

## Where we are

`crates/runtime/src/ir.rs` already has the Ignition bones:

- A `Step` enum (~90 variants): stack ops, unary/binary, member/private/super
  get+set+update+delete, destructuring, literals/arrays/objects/classes,
  calls/construct, control flow, try/catch/finally, loops, `using`, and the
  suspension machinery (`Yield`, `Await`, `yield*` drivers, async-for-of).
- `CompiledBody { steps, handlers }` with a handler table, and a `Vm` whose
  `run_inner_inner` is the dispatch loop.
- A `Compiler` that lowers the hard cases correctly: suspension paths,
  `yield*` delegation loops, async generators, `?.` chains
  (`compile_expr_force`), logical/conditional short-circuit with labels,
  assignments incl. `&&=`/`||=`, `using`/`await using` disposal, scopes.

The walker still owns the common case through two batching defaults:

1. `compile_expr` (ir.rs L4772):
   `if !expr_contains_suspension(expr) && !expr_may_short_circuit(expr) {
   emit(Step::Expr(expr.clone())) }` — any expression without `await`/`yield`
   /`?.` is cloned into a `Step::Expr` and tree-walked at runtime by
   `crate::expr::eval_expr`.
2. `compile_statements` (ir.rs L4206):
   `if !(stmt_contains_suspension(stmt) || stmt_contains_exit(stmt) ||
   async-gen return)` → `Step::Stmt(stmt.clone())`.

So today the policy is: compile only what *must* suspend; walk everything
else. Ignition is the inverse: compile everything; suspension is just more
bytecode.

## Cut 1 — compile everything (the actual win)

Make both gates unconditional and delete the fallbacks.

### Expressions: replace `Step::Expr` with real steps

`compile_expr`'s `_ => Step::Expr` fallback (L4998) currently catches:

| ExprKind | New step(s) |
|---|---|
| `Literal` | `Push` (value already known at compile time; `RegExp` literals compile the pattern once) |
| `Ident` | new `LoadIdent { name }` (resolve + TDZ check) |
| `This` | `ThisValue` (exists) |
| `Super` | unreachable alone; handled by member/call arms |
| `Function` / `Arrow` | new `CreateFunction { function }` (closure creation; arrows carry the same shape) |
| `MetaProperty` (new.target) | new `NewTarget`; `import.meta` already emits `ImportMeta` |

The gate at L4772 (`expr_contains_suspension` / `expr_may_short_circuit`)
is removed — everything compiles. The `?.`-chain logic stays (it already
compiles those), so `expr_may_short_circuit` becomes unused and is deleted
with its helper `expr_contains_suspension` (which is only a gate, not a
lowering).

### Statements: replace `Step::Stmt` with real steps

`compile_statement`'s `_ => Step::Stmt` fallback (L4432) catches:

| StmtKind | New step(s) |
|---|---|
| `Empty` | nothing |
| `Debugger` | nothing (host no-op, like today) |
| `FunctionDecl` | new `FunctionDecl { function }` (create closure, bind the hoisted name) |
| `ClassDecl` | already lowered at L4420 |

The batching gate at L4206 is removed; every statement lowers. The
`stmt_contains_suspension` / `stmt_contains_exit` predicates become dead.

### Delete the walker from the compiled path

- Remove `Step::Expr` and `Step::Stmt` variants and their `Vm` cases.
- `EnterBlock { stmts }` and `CatchBind { stmts }` embed statement lists the
  VM tree-walks for declaration instantiation — shrink them to what the VM
  actually needs (`EnterBlock { decls }`), compiling the bodies like any
  other block.
- `TaggedTemplate(quasi)`, `ObjectMethod*`, `ClassBegin/Finish` embed AST
  for runtime re-evaluation — the methods/classes are compiled bodies, so
  the VM creates closures from `CreateFunction`, not from embedded AST.
- `eval_expr` / `eval_statement` remain reachable from top-level scripts
  only if the top level stays walked; the plan is to compile top level too
  (it has no suspension, so it is the purest case — but it also touches the
  module harness, `install_harness_globals`, and `run_script`, so it is
  scoped separately in Cut 1b).

## Cut 2 — tighten the encoding (V8 `bytecodes.h`)

After Cut 1 the dispatch loop is a straight `match step` over small
operands. Borrow Ignition's specializations that don't need ICs:

- Zero-operand constants: `LdaUndefined`/`LdaZero`-style (`Push(Value::Undefined)`
  appears in `void`, `return;`, `yield` — hoist to a `PushUndefined` step or
  fold into the surrounding step).
- Small-int immediates: `LdaSmi`-style for literal `Number(i32)` (the tree
  walker currently allocates a `Value` per literal; keep the operand in the
  step).
- Fused unary: `Inc`/`Dec` replace `Push(1); Binary(Add)` in `compile_update`.
- Nullish/undefined tests: `JumpIfNull/Undefined` (we already have
  `JumpIfNullishKeep`; add the non-keep and the true/false variants for the
  `Test` context, like Ignition's `JumpIfToBooleanTrue/False`).
- Arity-specialized calls: `CallProperty0/1/2` and
  `CallUndefinedReceiver0/1/2` (V8 `CALL_PROPERTY_BYTECODES`) skip the
  args-vector build for the common arities.
- Constant pool: `LdaConstant` for interned strings (avoids embedding
  `JsString` clones in every step).

## Cut 3 — registers (deferred, optional)

Ignition's accumulator + register model (`bytecode-register-allocator.h`,
`bytecode-register-optimizer.cc`) cuts push/pop traffic: `Lda*` reads into
the accumulator, `Star` stores. The stack machine works; this is polish,
not the point. Do it only after Cut 1 lands and the bench shows the walker
overhead is gone.

## Validation

- `cargo clippy --workspace --all-targets -- -D warnings` clean, then
  `cargo test --workspace` green (the full engine suite — modules, async,
  generators, classes, `using` — runs on the compiled path after Cut 1).
- The test262 harness runs fixtures through `run_script`/`run_one_module`;
  after Cut 1 the whole corpus (48,622 fixtures) exercises the bytecode.
  Re-sweep `language` + `built-ins` + `annexB` after the change (the walker
  is shared, so the full areas are the regression check).
- `cargo run -p cli --release -- --bench` vs the pre-change snapshot; the
  gate is the perf.md milestone ("Bytecode VM replacing the tree-walker",
  hot-path ≥ 5x). Update `docs/perf.md` — its "no bytecode" description is
  already stale: the `Step` VM exists and runs generators/async today.

## Non-goals

- No ICs/feedback (that is the TurboFan tier-up story; out of scope).
- No GC rewrite; no NaN-boxing; no ropes (separate perf.md milestones).
- The regexp engine (`crates/regexp/src/engine.rs`) is a CPS tree-walker and
  is the same "compile everything" story in miniature (flat bytecode +
  explicit backtrack stack, V8 `src/regexp/regexp-bytecodes.h` +
  `regexp-interpreter.cc`). It is a separate workstream; its ~14 s/fixture
  cost only shows in the 586-fixture regExpUtils cluster and does not block
  runtime bytecode.
