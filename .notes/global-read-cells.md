# Global reads: the cell only warms for object records

**Status: LANDED (2026-09-16)** for the declarative half below; the nested-body
half (`clean_chain`, last section) is still open. Corpus rows added under
`tools/corpus/workloads/globals/`; the numbers under "The finding" are the
pre-fix ones, reproducible with the commands in `tools/corpus/README.md`.

## Landed

The cell now serves the global env's declarative record, following this note's
design with one simplification: instead of a *new* declarative epoch carried in
the cell, the global environment bumps the GLOBAL OBJECT's generation on every
declarative mutation (`GlobalEnv::bump_declarative_generation`, on create /
initialize / write / delete of its declarative record). The probe's existing
`(name, global_id, generation)` validation therefore covers both records with no
new cell field, no new ctx field and no codegen change. A declarative cell is
load-only (`slot = u32::MAX`), which also disables the compiled `StoreGlobal`
fast path — a `const`'s cell can never be written through a property slot.

Measured with the corpus runner (1M iterations, same machine and binary):

| workload | jit before | jit after | jitless before | jitless after |
| --- | ---: | ---: | ---: | ---: |
| `declarative_read.js` | 52.5 ms | **2.47 ms** | 96.6 | 17.0 |
| `hoisted_local.js` | 2.48 | 2.47 | 16.9 | 17.0 |
| `object_read.js` | 2.46 | 2.52 | 17.3 | 16.5 |

The three binding kinds are now the same speed — the 21x/25x gap this note
opens with is closed. The `globals` family gap went 30.94x -> 2.82x and the
corpus overall 34.05x -> 31.21x with 0 mismatches.

The version bump is now the correctness argument for BOTH records, so two
behaviours beyond this note's scope are pinned by new tests:

- a later script's `const K` invalidating a cell warmed from an earlier
  `globalThis.K` property
  (`installed_jit_declarative_binding_invalidates_a_warmed_object_cell`) — this
  one was a real hole before the fix;
- a `let` written by a called function mid-loop
  (`installed_jit_declarative_read_sees_a_callees_write`).

Both fail if the bump is removed (verified by stubbing it out). Full detail,
including the audit of every mutation path into the global record, is in
`.notes/perf.md` (2026-09-16).

## The finding

A read of a global binding that lives in the global env's **declarative record** —
any top-level `const`, `let`, `class`, or function-declaration *name* — costs
**~63 ns per read inside a compiled loop**, while the same loop reading the same
value from a local, or from a global **object-record** property, costs **~2.5 ns**.
That is a 25× gap, inside machine code, on one of the most common operations in
bundled application code.

`tools/corpus/workloads/globals`, 1,000,000 iterations, result identical in all
four engine/modes (`1500001500000`):

| workload | slag jit | slag jitless | node |
| --- | ---: | ---: | ---: |
| `declarative_read.js` — top-level `const K`, read in the loop | **63.5 ms** | 96.6 | 0.58 |
| `hoisted_local.js` — same value read once into a local | **2.48 ms** | 17.2 | 0.59 |
| `object_read.js` — `globalThis.K`, an object-record property | **2.46 ms** | 17.3 | 86.5 |

workload, 1M iterations | slag jit | slag jitless | node |
| --- | ---: | ---: | ---: |
| top-level `const` read in the loop | **63.5 ms** | 96.6 | 0.58 |
| the same value from a local | **2.5** | 17.2 | 0.59 |
| the same value as `globalThis.K` | **2.5** | 17.3 | 86.5 |
| same loop, literal bound (no global) | 1.06 | 10.1 | 0.18 |
| `Math.PI` in the loop body | 2.12 | 16.7 | 25.7 |
| `(i+1) ** 0.5` / `Math.sqrt(i+1)`

Slag is *better* than node on the object-record row (the cell makes it a native
load; node pays an unfolded property read) and loses 100× on the declarative row
(node folds a top-level `const` away entirely). The scratch set the rows came from
also showed the effect is about the *read*, not the loop head: the same `const`
read in the loop **body** (`13_body_scriptconst.js`) measures 18.08 ms against
18.35 ms for the same read as the loop **bound** — and both are ~17× the same loop
with a literal bound (1.06 ms).

## Why, from the code

`crates/runtime/src/jit.rs::load_ident` warms the JIT's `GlobalValueCell` — the
direct-mapped cell the compiled `LoadIdent` probe checks before falling back to
the full resolve — **only** for object-record bindings (the code below is the
pre-fix shape; the Landed section above records what replaced it):

```rust
// Warm the JIT's global fast cell when the resolved binding is a global
// OBJECT-record (var/function/undeclared) data property ...
// A DECLARATIVE-record binding (a top-level `let`/`const`/`class`) or any
// other env never warms: the probe validates only the cell's name and
// the global object's version, which a declarative shadow does not disturb.
```

So every declarative read in compiled code takes the slow path:
`resolve_binding` + `get_value`. Both are cheap by themselves (~57 ns together,
measured), but they run per read, and `HoistGlobalGuard` (the existing LICM for
invariant global cells) has nothing to hoist because the cell never filled.

The comment's soundness argument is the whole fix shape: the probe validates the
cell's name and the **global object's** generation. A declarative binding can be
shadowed later (a *later script* may delete the binding when no longer referenced
and re-declare the name), and that event does not disturb the global object, so a
validated cell could serve a stale value. Warming for declarative bindings needs
something the probe can check that a script-level re-declaration *does* disturb —
a declarative-record generation/epoch on the global env, carried in the cell and
compared in `LoadIdent`'s compiled probe. Once the cell warms, the existing
`HoistGlobalGuard` should pull invariant reads out of loops for free.

## The nested-body half: LANDED (2026-09-16)

This section began as the design for closing the gap; it is kept as the record,
with the corrections the implementation forced.

Landed result on `globals/nested_read.js` (a helper that CAPTURES a binding from
the function it is nested in — the mod-kernel shape; see the correction below):

| row | before | after |
| --- | ---: | ---: |
| `nested_read.js` jit | 130.2 ms | **2.5 ms** |
| `nested_read.js` jitless | 125.9 ms | **18.1 ms** |
| `object_read.js` (top level) | 2.5 ms | 2.5 ms |

The `globals` family is now four workloads at one speed: 2.08x over V8 with the
interpreter at 0.78x (it was 30.94x / 2.39x).

What landed:

- The verdict below, as designed: `Vm::global_reads_are_unshadowed` walks the
  body's chain once per run for exactly the names it reads (`CompiledBody::
  `ident_names`), and the flag (`chain_globals_ok` was the working name;
  `globals_unshadowed` shipped) replaces `clean_chain` in both engines and in the
  LICM guard. A body on the bare global record still answers without walking.
- One more thing the design did not foresee, which the relaxation EXPOSED and
  this change also fixes: an in-place VALUE write deliberately does not bump the
  receiver's generation (the L1c no-bump discipline), but the name-keyed global
  cell is validated BY that generation — so `globalThis.value = ''`, written by
  compiled or interpreted in-place store code, left a global read serving the
  pre-write value. `Vm::refresh_global_read_cell` now fronts the new value at the
  unchanged generation from every in-place store path
  (`warm_store_put`/`warm_store_map_put`/`warm_store_fallback`/
  `warm_store_direct_put` and the JIT's `set_member_slot`). The cells were only
  ever probed by bare-global-chain bodies before, which is why nobody hit it;
  test262's `language/types/reference/get-value-prop-base-primitive-realm.js`
  found it within one sweep (it reads a global through `eval` in a second realm,
  writes it with a member store, and reads again).

**Why a chain-shape gate is unsound: the cell is shared by name.**
`agent.global_value_cells` is ONE direct-mapped table, keyed by the name, and the
probe checks only `(name, global_id, generation)` — nothing body-specific. So
"this body's chain is closed" is not enough. The reachable form of the hazard is
a DYNAMICALLY bound name in the chain — an eval-injected `var`, or a `with`
object's property:

```js
var x = 1;
function top() { ... x ... }              // warms the cell for `x` with 1
function outer() {
  eval('var x = 5');                      // injects `x` into outer's own env
  function inner() { ... x ... }          // must read 5, not the cell's 1
}
```

A **declared** enclosing binding is NOT this hazard: the compiler resolves it as
a capture (a context slot), so it never becomes a `LoadIdent` and never consults
a cell. The first cut of the regression test used `const x = 2` in the wrapper and
passed under a chain-shape-only gate precisely because of that — the shipped test
uses the `eval` form, which fails under that gate (verified). The verdict has to
be **per name and per run** either way.

**The verdict.** At run entry, for a body that (a) has at least one `LoadIdent`
read and (b) whose running env is not itself the global record, walk the chain
from the body's env outward once per distinct read name:

- a `with` record anywhere ⇒ not clean — its object can gain the property later,
  and gaining it does not move the global's generation (this is exactly the
  `installed_jit_ident_read_with_scope_shadow_is_respected` case);
- a record *before* the global record that `has_binding(name)` ⇒ not clean — the
  resolve would stop there;
- the walk must reach a `Global` record ⇒ otherwise not clean (a module's chain,
  or a host function whose chain ends at null).

If ANY name is not clean, the flag goes off for the whole run: every `LoadIdent`
probe and the LICM guard degrade to today's behaviour. That is the conservative
degradation — a body that shadows one global name and reads another loses the
fast path for the second, i.e. behaves as it does now — and it is what keeps the
mechanism to a single bool.

**Why the verdict holds for the whole run.** A body's own statements cannot add a
record to its chain: a certified body never creates an env (`env_constant`;
`with`, `catch` and eval are scanner rejects), and it cannot run an owner body
that could — an owner of a chain record is a suspended caller while this body
runs, and a suspended caller cannot execute. So a direct eval in an owner body
can only inject a `var` into that env *before* this run entry, which the walk
sees. A `with` object gaining a property mid-run is the one live hazard, and the
`is_with` check is precisely what refuses to trust such a record. Every other
change to what the name resolves to is a global-record change, covered by the
cell's own generation check (now including the declarative record, above).

**What changes.**

1. `CompiledBody` gains `ident_names` — the distinct `LoadIdent` atoms, computed
   in the same scan as `body_has_loop` in `compile_body`/`compile_statements`.
   Empty means nothing to check, and it is also what keeps the common case free:
   a body on the bare global record never walks at all, the existing check
   already answers.
2. The run-entry computation in `run_jit_body` / `run_jit_resume` and the
   interpreter's `run_inner` becomes one call to a `Vm` helper
   (`global_reads_are_clean(env, &body.ident_names)`); `run_jit_leaf` keeps its
   uniform value (a leaf never contains `LoadIdent`).
3. The flag keeps its shape — one bool in `Vm` and `JitCallContext` — so the
   compiled `LoadIdent` arm, the LICM guard and `warm_global_cell`'s gate need no
   new machinery. It should be RENAMED (`chain_globals_ok`), because "clean
   chain" is no longer what is being tested.
4. `warm_global_cell`'s gate stays as it is: the resolve must have landed on the
   `Global` record, so an intervening binding never warms the cell, which is what
   makes a served value always the global record's.

**Cost.** The walk is once per call, only for a nested body that reads an
identifier: a handful of `has_binding` probes (a linear scan of a wrapper's
bindings, or a property lookup on the global object) against saving a full
resolve on every iteration of a hot loop.

**Tests a wrong implementation fails** (the first two are the point of the
design):

- a dynamically bound name in the chain is never served from another body's cell
  — the `eval` shape above (`installed_jit_nested_ident_read_is_not_served_from_
  a_shadowing_chain`), verified to fail under a chain-shape-only gate;
- a statically declared enclosing binding stays a capture and is served from the
  slot, not the cell (`installed_jit_enclosing_declarations_are_captures_not_
  global_reads`) — it pins the shape that must NOT start consulting a cell;
- a nested helper reading a global property in a loop returns what the top-level
  reader of the same name returns (`a_nested_global_read_warms_the_value_cell`);
- a nested read still sees a callee's write (the invalidation survives the
  relaxed gate) — `installed_jit_nested_global_read_sees_a_callees_write`;
- the walk itself, name by name, including the `with` and no-global-record cases
  (`global_reads_are_unshadowed_checks_each_name_against_the_chain`);
- an in-place member store to a global refreshes the read cell
  (`installed_jit_member_store_to_a_global_refreshes_the_read_cell`), verified to
  fail with the refresh stubbed out.

The existing `with`-closure e2e test
(`installed_jit_ident_read_with_scope_shadow_is_respected`) stays green.

**Alternatives rejected.** (a) A per-name poison table in the ctx — finer, keeps
the fast path for the clean names of a mixed body, but a direct-mapped fold can
collide, and a collision is silently UNSOUND; if it is ever wanted the fold must
be exact or the overflow must degrade to the single flag anyway, so it buys
little over this. (b) Telling the loader that a wrapper's bindings cannot shadow
— the same knowledge moved to a different site, and it does not cover a closure
created inside a `with`. (c) Recording the resolving record's identity in the
cell and validating it per read — that needs the chain walk per read, which is
the cost being removed.

## Reproducing

```
cargo build --release -p cli
target/release/slag --corpus tools/corpus/workloads/globals
target/release/slag --corpus tools/corpus/workloads/globals --jitless
node tools/corpus/run_node.js tools/corpus/workloads/globals
```

The scratch set the rows were derived from (12 shapes: locals, script `const` and
`let`, `Math.PI`, `globalThis.G`, a function-declaration read, a script-function
call, `** 0.5` vs `Math.sqrt`, a kernel called 1000×, and the hoisted-local
control) lives in `scratch/jitprobe/` — gitignored, kept for re-measurement.
