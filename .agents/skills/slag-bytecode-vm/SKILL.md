---
name: slag-bytecode-vm
description: "Load when working on Slag's bytecode VM and compiler (crates/runtime/src/ir.rs) — the Step enum, the Vm dispatch loop, compile_expr/compile_statement lowering, or the compiled-path stack protocol. Documents the non-obvious traps: optional-call short-path stack discipline, assignment-reference timing, super base capture before key conversion, destructure/for-of close semantics, template cache keys, and the bench-noise reality."
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

## Bench reality (Cut 2)

`cargo run -p cli --release -- --bench` bounces ±15% on this machine (the
array-iteration benchmark has ranged 13.7–19.1s across identical runs) —
only judge consistent multi-run deltas, never a single run. An empty
1M-iteration `var` loop alone is ~630ms of the ~1.19s arithmetic bench: the
loop machinery (dispatch + env resolution) dominates, so encoding fusion
moves numbers within noise, and the perf.md ≥5x gate needs structural work
(registers/accumulator, i.e. the plan's Cut 3), not more step fusion.

## Validation loop

`cargo clippy --workspace --all-targets -- -D warnings` clean, then
`cargo test --workspace` green — then REBUILD the sweep
(`cargo build --release -p test262`) and run the full areas; the compiled
path is shared, so a local fix can regress anywhere. For an A/B regression
check, diff the fail+crash union against the parent worktree at
`C:/Users/T/Desktop/jsrt-parent` (at `8c2f0cf`; its sweep binary must be
rebuilt from that source). See the `slag-conformance` skill for the sweep
workflow and the load-dependent classification trap.
