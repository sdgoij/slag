---
name: slag-jit
description: "Load when working on Slag's Cranelift JIT (crates/jit/src, crates/runtime/src/jit.rs, JIT-visible crates/runtime/src/ir.rs) — helper ABI registration, emit_step lowering, block sealing, dispatch sentinels, leaf-call probe, certified iterators (for-of/for-in/AsyncForOf*), suspension (Yield/Await resume dispatch), super access, new.target, or destructuring (Destructure* steps, flat binds). Traps: the four-file helper mirror, the pending-error ABI, the dispatch chain, the element-via-work-stack convention, the certified-callee lane, the ForOfBegin boundary span, the compiled for-of close requirements, the sealed-block/back-edge rules, the for_of_advance core contract, resumable-body rules, the destructure rules (flat binds, for-head trap, close gates), the script completion-register writes, and the Cut 69 compile threshold (the lookup_info gate, body_has_loop↔step_targets sync, the call_slow→try_jit_leaf chain)."
---

# Slag JIT traps

The JIT (`crates/jit/src`) compiles certified `CompiledBody`s to Cranelift
machine code. `crates/runtime/src/jit.rs` owns the runtime side: the
`JitCallContext`, the `JitSlowPaths` helper table, and the helper
implementations that route compiled code back into the interpreter's
machinery. This skill covers the traps that cost real debugging time;
the interpreter-side VM traps live in `slag-bytecode-vm`.

## Where the traps live

The detail is split by area so you load only what the task touches. Every file
is under `reference/` beside this one. **Section numbers did not change in the
split** — an older note that cites `slag-jit` §18 means §18, wherever it lives:

```
§1-§4    reference/foundations.md
§5-§14   reference/syntax-features.md
§15      here
§16-§18  reference/call-lanes.md
§19      reference/member-reads.md
§20-§21  reference/call-lanes.md
§22-§24  reference/opt-tier.md
§25      reference/call-lanes.md
§26-§29  reference/opt-tier.md
§30      reference/member-reads.md
§31-§34  reference/opt-tier.md
§35      reference/member-reads.md
```

| file | what is in it | open it when |
|---|---|---|
| [`reference/foundations.md`](reference/foundations.md) | the four-file helper mirror, helper signatures, the pending-error ABI and dispatch sentinels, the sealed-block rules | you add or change a helper, a call ABI, or a jump target |
| [`reference/syntax-features.md`](reference/syntax-features.md) | for-of/for-in lowering and close semantics, the register body, suspension (generators/async), destructuring, arguments/`typeof`, `super`, `new.target`, fused global/slot calls and the completion register | you touch a `Step` family or its interpreter counterpart |
| [`reference/call-lanes.md`](reference/call-lanes.md) | the leaf record and its revalidation, the compile threshold in `lookup_info`, the scope-vs-`emit_step` gates, the compiled self-call, the certified-callee lane, the optimizing tier's call lane | a call is slow, a body does not compile, or you touch `call_slow` |
| [`reference/member-reads.md`](reference/member-reads.md) | the read-side value cells and the store discipline, the helper-traffic census, the prototype-chain cell (identity and map-keyed arms, function receivers) | a member read misses its cell, or you add a cache |
| [`reference/opt-tier.md`](reference/opt-tier.md) | the speculation guard (`vm.ip` is not per-step), splice rules, the lift census, for-of/for-in lifting, the fused head, computed reads, the LICM hoist guard, read guards vs the leaf lane, the GC safe point, nested-run state restore | you work in `crates/jit/src/opt*` |

## The traps that cost the most, in one line each

Each of these has a section in the file named; none is guessable from the code.

- **A helper touches four files** (`foundations.md` §1) and its arity slip is
  silent — the runtime reads machine-code argument slots.
- **Never add a jump into a sealed block** (`foundations.md` §4): Cranelift panics
  at compile time in `ssa::declare_block_predecessor`, and the *block name* you
  reuse is often the culprit (a duplicate `gated` did exactly that).
- **A body that bails the optimizing tier looks fine** (`opt-tier.md` §24,
  `call-lanes.md` §17): `JIT_DUMP_CLIF=1` prints `opt bail: Step("…")`, and a
  missing `emit_step`/lift arm is a silent 10-30% loss on exactly those rows.
- **A probe that never hits is indistinguishable from a fast one**
  (`member-reads.md` §35.1): check the table pointer *and* the slot arithmetic
  against the runtime's index function before touching the gates.
- **`vm.ip` is not the current step** (`opt-tier.md` §22): it is only meaningful
  after a deopt returned to the runtime.
- **The store paths keep the read cells current, and they do not always bump the
  generation** (`member-reads.md` §19) — a value cache keyed by anything but the
  atom cannot be refreshed.

## 15. Validation

`cargo clippy --workspace --all-targets -- -D warnings` clean, then
`cargo test --workspace` green — then REBUILD the sweep
(`cargo build --release -p test262`) and run the full areas (see the
`slag-conformance` skill). The JIT (default) baselines: `language` 23721/0/0/0,
`annexB` 1086/0/0/0, `built-ins` ~23210 pass / 0 fail with 440–450
pre-existing hang wobble on the slow RegExp property-escape /
CharacterClassEscapes / decodeURI / TypedArray / Temporal clusters (load
wobble, not a regression — diff the fail+crash union against a parent
worktree). A JIT e2e test lives in `crates/jit/src/lib.rs`
(`with_jit_agent` runs a script with the real runtime helpers and reports
the compiled-body count); the `installed_jit_*` tests are the behavioral
suite — new steps need one, including the iterator-close shapes
(break/return close, `next()` error does not, body error does).
