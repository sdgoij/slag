# Bytecode for everything: the runtime IR, Ignition-style

Status: **Cut 1 landed.** Every expression and statement compiles to `Step`
bytecode, and ordinary function calls/constructs plus top-level scripts run
on the `Vm` dispatch loop — the tree-walker no longer executes normal
program flow. The remaining work toward the perf.md gate (hot-path ≥ 5x) is
Cut 2's encoding tightening; Cut 3 (registers) stays deferred.

The reference is the V8 checkout in `v8/`:
`src/interpreter/bytecodes.h` (instruction list), `bytecode-generator.cc`
(AST lowering), `bytecode-register-allocator.h` + `bytecode-register-optimizer.cc`
(register allocation), `interpreter.cc` (dispatch).

## Where we are

`crates/runtime/src/ir.rs` holds the Ignition bones, fully wired:

- A `Step` enum (~150 variants): stack ops, unary/binary,
  member/private/super get+set+update+delete, destructuring, literals/
  arrays/objects/classes, calls/construct, control flow, try/catch/finally,
  loops, `using`, the suspension machinery (`Yield`, `Await`, `yield*`
  drivers, async-for-of), and the reference-resolution family
  (`ResolveVarIdent`/`GetVarReference`/`PutVarReference*` +
  `ResolvePrivateRef`/`ResolveMemberRef*`/`ResolveSuperRef*`).
- `CompiledBody { steps, handlers }` with a handler table, and a `Vm` whose
  `run_inner_inner` is the dispatch loop.
- A `Compiler` that lowers the hard cases correctly: suspension paths,
  `yield*` delegation loops, async generators, `?.` chains, logical/
  conditional short-circuit with labels, assignments incl. `&&=`/`||=`/
  `??=` (member/private/super targets included), `using`/`await using`
  disposal, scopes.

The old batching defaults — `Step::Expr`/`Step::Stmt`, which cloned
suspension-free subtrees and tree-walked them at runtime — are deleted.

## Cut 1 — compile everything (done)

Both batching gates were made unconditional and the fallbacks deleted.

### Expressions

The `_ => Step::Expr` fallback is gone; every expression lowers:

| ExprKind | Step(s) |
|---|---|
| `Literal` | `Push` (values known at compile time; `RegExp` literals construct at runtime) |
| `Ident` | `LoadIdent { name }` (resolve + TDZ check) |
| `This` | `ThisValue` |
| `Super` | member/call arms (never alone) |
| `Function` / `Arrow` | `CreateFunction` / `CreateArrow` (closure creation) |
| `MetaProperty` | `NewTarget`; `import.meta` emits `ImportMeta` |

### Statements

The `_ => Step::Stmt` fallback is gone: `Empty`/`Debugger` lower to
nothing, `FunctionDecl` to a `FunctionDecl` step (Annex B statement-position
form included), `ClassDecl` inline.

### Top level and ordinary functions

- `eval_program` compiles and runs top-level scripts through the VM (the
  plan's "Cut 1b" scope: `compile_statements` + `Vm::start`).
- `register_function`/`instantiate_arrow` compile **every** body at creation
  (previously only async/generator); `ordinary_call` and `ordinary_construct`
  execute the compiled body on the VM. The compiled body is shared as
  `Rc<CompiledBody>`, so the per-call record read does not copy the steps.

### Dispatch tightening (Cut 1 follow-ups)

- The dispatch loop matches on `&Step` instead of cloning the enum each
  iteration, and the per-step context env sync is skipped when the
  environment has not changed (an `Rc` pointer comparison).
- New steps from the conformance campaign: `Dup2` (duplicate the (base,
  key) pair for computed compound assignments), `Break`/`Continue` (route
  through pending finallys), `GetMemberComputedKeep`/`GetSuperComputedKeep`
  (convert the key once, after the nullish check), `DeleteSuper` (the
  spec's ReferenceError), `InvalidAssignmentTarget` (Annex B function-call
  assignment targets), and the `GetVarReference`/`PutVarReferenceOp`/
  `UpdateVarReference`/`Resolve*Ref` family (resolve an assignment target's
  reference before its RHS evaluates).

### The walker-masked bug campaign

Removing the walker from normal execution exposed a long tail of bugs the
batching used to mask: assignment-reference timing (`with`-delete
semantics), member/private/super `&&=`/`||=`/`??=` short-circuits,
private-member assignment/update lowering, computed-compound key conversion
order, destructure/for-of iterator-close semantics (incl. `next()`-error
exemptions and double-close), catch environment unwinding, `break`/
`continue` through finallys, throw-from-catch finally routing, super base
capture before key conversion, template-object caching across compilations,
Annex B call-assignment targets, and `using` disposal on abrupt errors.
All fixed; the full sweeps are at zero regressions vs the parent commit
(see Validation).

## Cut 2 — tighten the encoding (V8 `bytecodes.h`) — partial

The dispatch loop is a straight `match step` over small operands. Borrow
Ignition's specializations that don't need ICs. **Landed so far:**

- **Small-int immediates** — `BinaryImm { op, imm }` (commit a02b451): a
  binary op whose right operand is a `Number` literal keeps the constant in
  the step, skipping the `Push`/`Pop` round-trip (`i * 2`, `i < 1_000_000`,
  `x + 1` in the hot loops).
- **Arity-specialized calls** — `CallFast { argc, direct_eval }`: calls
  with 0-2 plain (non-spread) arguments keep them on the value stack and
  the VM reads the argument slice in place, skipping the args-vector build
  and the per-call `Vec` allocation (V8 `CallProperty0/1/2` +
  `CallUndefinedReceiver0/1/2`). Constructs and super calls keep the vector
  form (`compile_arguments(args, allow_fast=false)`).

Landing the fast-call path exposed a pre-existing stack leak in the
optional-call tail: `compile_optional_call_tail`'s nullish short path
popped two values (the callee and its dup) but not the receiver, so
`null?.(x) === undefined` threw and every later stack read shifted. The
short path now pops three and leaves exactly `[undefined]`; seven
regression tests cover the nullish and non-nullish optional-call cases.

Full release sweeps after the landing: zero regressions vs the parent in
all three areas (language 152 fail vs 274, built-ins and annexB unchanged;
the only deltas are the known decodeURI batch-death classification). The
bench numbers are within run-to-run noise on every metric; string concat
moved ~5% lower (169ms → ~160ms, the fused loop-test compare). The ≥5x
gate is still open.

**Still open (deferred or low-leverage):**

- Zero-operand constants: `LdaUndefined`/`LdaZero`-style (`Push(Value::Undefined)`
  appears in `void`, `return;`, `yield` — a `PushUndefined` step saves one
  match arm; the value is already an inline `u64`, so the win is minimal).
- Fused unary: `Inc`/`Dec` replace `Push(1); Binary(Add)` in `compile_update`
  — already effectively landed: `compile_update` emits the dedicated
  `Update*` steps with an `UpdateOp` operand, no `Push(1)` involved.
- Nullish/undefined tests: `JumpIfNull/Undefined` variants for the `Test`
  context, like Ignition's `JumpIfToBooleanTrue/False` (the keep-variants
  exist; the plain consumed-value variants were added and removed during the
  conformance campaign — reintroduce only with a caller).
- Constant pool: `LdaConstant` for interned strings (identifiers already
  embed `AtomId`, not `JsString` clones, so the win is limited to string
  literals embedded in `PushStr`).

## Cut 3 — registers (deferred, optional)

Ignition's accumulator + register model (`bytecode-register-allocator.h`,
`bytecode-register-optimizer.cc`) cuts push/pop traffic: `Lda*` reads into
the accumulator, `Star` stores. The stack machine works; this is polish,
not the point. Do it after Cut 2 lands and the bench still misses the gate.

## Validation (Cut 1 state)

- `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo
  test --workspace` green (18/18 suites — modules, async, generators,
  classes, `using` — all on the compiled path).
- The test262 harness runs fixtures through `run_script`/`run_one_module`;
  the whole corpus exercises the bytecode. Full release sweeps vs the
  parent commit (`8c2f0cf`): **zero regressions** in all three areas, with
  122 net fixes in `language`:
  - language (23,724 fixtures): 152 fail vs the parent's 274
  - built-ins (23,812): 7 fail, identical to the parent
  - annexB (1,086): 1 fail, identical to the parent
- `cargo run -p cli --release -- --bench` is at or near the pre-compile
  walker baseline on every benchmark (arithmetic 1.15s vs 1.14s, property
  1.47s vs 1.50s, string concat 0.169s vs ~0.15s, array 13.4s vs 13.6s,
  calls 4.34s vs 4.36s); the perf.md gate (hot-path ≥ 5x) needs Cut 2,
  whose first two items (immediates, fast calls) landed without moving the
  numbers beyond noise.

## Non-goals

- No ICs/feedback (that is the TurboFan tier-up story; out of scope).
- No GC rewrite (a separate perf.md milestone); the NaN-boxing, shapes/IC
  cache, and string-rope milestones already landed.
- The regexp engine (`crates/regexp/src/engine.rs`) is a CPS tree-walker and
  is the same "compile everything" story in miniature (flat bytecode +
  explicit backtrack stack, V8 `src/regexp/regexp-bytecodes.h` +
  `regexp-interpreter.cc`). It is a separate workstream; its ~14 s/fixture
  cost only shows in the 586-fixture regExpUtils cluster and does not block
  runtime bytecode.
