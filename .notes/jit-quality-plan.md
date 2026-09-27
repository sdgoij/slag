# JIT execution quality: the inline-coverage plan

The plan of record for making a compiled body fast, as opposed to compilable.
`.notes/non-leaf-jit.md` is the coverage plan and reached its own definition of
done (0 emitter refusals) — this plan exists because coverage turned out not to
be the thing, and because the architecture below was read out of the code
rather than inferred from timings.

Status: Stage 0 is a **code audit, done 2026-09-28**. Result: the JIT is a
per-`Step` lowering with hand-written inline fast paths for a named subset and
`call_slow` into a 131-entry Rust helper table for everything else — **compiled
dispatch with an inlined core**, not a typed-IR compiler. §3 is the gap
inventory that follows, and it is the work list. No code changed.

Supersedes the first draft of this document, which concluded from `--jit-bench`
that the compiled tier was "node-class and not the problem". That was wrong for
a stated reason: the suite's shapes are the ones already inlined, so it detects
regressions and cannot judge the design. The correction is recorded rather than
edited away — it is the reason Stage 0 is now a code read.

## 1. Why this exists

The JIT's coverage work finished with the picture unchanged: 497 bodies
compiled in a forced `deno test` check and the JIT **14% net-negative** there,
while a plain leaf loop is 2.4× faster compiled
(`.notes/non-leaf-jit.md` §1/§6). Four "optimization" slices since then moved
individual corpus rows 17-46% and moved nothing else. The questions this plan
has to answer are therefore **architectural**, and the way to answer them is to
read what the compiler actually emits.

Two measurement instruments exist and neither answers it:

- `--jit-bench` (the engine's own JIT-vs-interpreter suite) reports a median
  ratio of 0.19 — 5.3× over the interpreter, 10-12.5× on loops, reads and leaf
  calls, and 1.8-2.9× on the call-shaped rows (`non-leaf call` 1.8×,
  `builtin call` 2.6×, `apply` 2.9×, `compound assign` 2.0×, `typed-array
  write` 2.3×). **These rows are exactly the shapes the inline subset covers**,
  so the good numbers are a reiteration of the design, not evidence about it,
  and the suite's job is regression detection.
- The corpus (77 rows, jit vs jitless) is a workload-mix number: 36 of the rows
  are the builtin-bound opcost family, where both modes are timing Rust. Its
  1.6× median says nothing about compiled-code quality.

So the plan's premise comes from the code (§2) and its evidence has to come
from **real-world code**, because both synthetic instruments are biased toward
shapes the engine already handles.

## 2. The architecture, read from the code

- **The design statement** is `crates/jit/src/compiler.rs:1-22`: "The lowering
  mirrors the interpreter's semantics op for op — the two number fast paths
  (the inline tag checks are `bits & TAG_MASK != TAG_PREFIX`, two instructions
  on the NaN-boxed `Value`) and the slow paths through the [`JitHelpers`]
  table. Anything unsupported bails … and the body runs on the interpreter."
  That is compiled dispatch with inline fast paths, by construction.
- **The slow table is the general path.** `JIT_SLOW_PATHS`
  (`crates/runtime/src/jit.rs`) carries **131** entries (the `Helper` enum's own
  variant count), and essentially every
  non-trivial `Step` has exactly one `emit_step` arm that calls one of them.
  A helper re-implements the interpreter's own op semantics, including its
  validation.
- **The inline subset** is the machinery the emitter carries directly:
  numeric tag checks, arithmetic, comparisons and truthiness
  (`emit_binary`/`emit_binary_known`/`emit_arith`/`emit_acc_binary`/
  `emit_num_slot_rmw`/`emit_rel_test*`); the fused loop counter in an `f64`
  Cranelift variable (`emit_fast_loop_*`); **register-op bodies**
  (`Step::RunRegBody` → `emit_leaf_op`, a `LeafOp` stream over
  `Acc`/`Spilled`/`Reg`/`Context`/`PerIter`/`Const` operands — the one real
  "compiled execution" path, and it covers the linear-accumulator loop shape);
  **named** member reads and writes through warm cells with shape probes
  (`emit_member_cell_read`/`emit_member_cell_probe`/`emit_shape_store_probe`/
  `emit_validated_member_store`/`emit_deferred_hole_fill`); global reads through
  value cells (`emit_global_read`); dense array append
  (`emit_dense_array_append_inline`); typed-array store and `length`
  (`emit_typed_array_store_inline`/`_length_inline`); and **leaf** calls
  (`emit_call`'s probe + `emit_leaf_call_tail`).
- **Everything else is a helper call**, and three of them are structural rather
  than a long tail:

  | surface | what the emitter does | evidence |
  |---|---|---|
  | computed member **read** (`a[i]`, `s[i]`, `o[k]`) | `call_slow(GetMemberComputed)` — no cell, no probe | compiler.rs:4679 |
  | computed member **write** | dense-append inline → typed-array inline → `SetMemberComputed` | compiler.rs:8036 |
  | any **non-leaf call** | `call_slow` (only leaves inline) | compiler.rs:3618 |
  | array literal | `ArrayBegin` + **one helper per element** + `ArrayEnd` | compiler.rs:6405-6416 |
  | object literal | `ObjectBegin` + "one helper per step" | compiler.rs:6436-6440 |
  | string concat | `ConcatStr` helper (except the builder path) | helper table |
  | `for-in`/`for-of` | all helpers (`ForIn*`/`ForOf*`) | helper table |

- **The cells are small and direct-mapped**: `MEMBER_CELLS = 16`
  (`ir.rs:2151`), `GLOBAL_CELLS = 256` (`ir.rs:2451`), `LEAF_CACHE = 16`
  (`ir.rs:2459`). A collision is a fallback to the helper, which is correct but
  slow — and it is why the same body can measure fast alone and slow inside a
  suite (the `slag-bytecode-vm` skill records exactly that artifact).
- **There is no type-feedback or deopt framework.** A fast path that misses
  calls the helper, which re-does the whole op; nothing records what a site has
  seen, and nothing re-specializes. The invalidation substrate exists
  (generations, the member/global cells, the leaf epoch), but there is no
  feedback loop on top of it.

## 3. The gap inventory (the work list)

Ordered by how much real code should touch them, each a concrete emitter
change with its own measurement:

- **G1 — computed-key access has no inline path.** `a[i]`/`s[i]`/`o[k]` reads
  are a helper round-trip each; the write side already has two inline fast
  paths. This is the largest single surface, and it subsumes the earlier
  typed-array `element_read` finding (~90 ns/element through the helper vs
  ~13-23 ns for a computed read that hits the cell machinery).
- **G2 — only leaf calls inline.** Every ordinary call in compiled code pays a
  `call_slow` round-trip; `--jit-bench`'s `non-leaf call` (1.8×) against
  `function calls` (7.7×) is that gap in one pair.
- **G3 — literals are helper sequences.** An array of *n* primitives costs
  *n*+2 helper calls; an object, one per property. Literals are ubiquitous in
  real code.
- **G4 — iteration is all helpers**, so every `for-of` step is a round-trip.
- **G5 — string building** (`ConcatStr`) is a helper outside the builder path.
- **G6 — cell capacity.** 16-entry member and leaf cells collide at realistic
  site counts; the fix is capacity (and/or a memo that does not degrade to the
  helper) rather than a new mechanism.
- **G7 — the register path is one shape.** `RunRegBody`/`LeafOp` is the only
  generalized "compiled execution"; widening what lowers to it is the
  structural version of G1-G5.
- **G8 — no feedback.** Any of G1-G6 done as a bespoke fast path repeats the
  existing pattern; doing them with a per-site feedback record is what makes
  the coverage systematic (§5).

## 4. How this gets measured (and what has been ruled out)

- **Real-world code is the evidence.** The two synthetic instruments are
  biased (microbenchmarks: inlined shapes; the corpus: builtin-heavy). The
  measurement this plan needs is a profile of **real** JS — where the time goes
  between inline paths and helper calls — plus wall time on workloads that are
  not what the JIT was tuned on.
- **The helper-share instrument does not exist and is Stage 1 of the work.**
  `JIT_DUMP_CLIF` prints bail/skip lines, not IR, so nothing today reports how
  much of a compiled body's execution is `call_slow`. The cheap instrument is a
  temporary per-body counter of emitted calls in `crates/jit/src/compiler.rs`
  plus a per-helper invocation tally in `crates/runtime/src/jit.rs` — the
  project's established temporary-instrument method (the census and the
  `SLAG_NO_*` probes are the precedent), removed again once it has answered.
  Without it, G1-G7 are ordered by reasoning rather than by data.
- **`--jit-bench` and the corpus stay, as regression detectors**, not as
  quality evidence: `--jit-bench` must not regress, and the corpus parity run
  must stay at 0 mismatches.
- **Release only.** Debug changes the program being measured (a 128-step
  compile cap against release's 1024, `debug_assertions` on including the A2
  write-barrier verifier, unoptimized helpers). The notes already carry the
  precedent ("the cap was sized on a debug build, where Cranelift is ~10× more
  expensive").
- **Deno is out of this arc.** It surfaced the performance problem (not for the
  first time) but it is not the subject: its tsc workload is `.d.ts` loading
  and string/`Map`/`Set` builtins, which no emitter arm touches, and it was
  measured net-negative with the JIT on. Nothing here is justified by it, and
  no deno probe is part of any stage.

## 5. Stages

- **Stage 1 — the instrument.** The helper-call counter (§4), run over
  real-world JS, producing the table this plan currently lacks: helper
  invocations per body, ranked by total time. Gate: the table exists and the
  gap order in §3 is confirmed or rewritten by it.
- **Stage 2 — G1, computed-key access.** An inline path for the computed read
  mirroring the existing computed store (dense/typed-array fast paths first,
  helper fallback), with a per-site shape guard. Gate: the instrument shows
  the helper share for computed reads collapse; `--jit-bench` and the corpus
  do not regress; the whole corpus and both corpora sweeps stay at baseline.
- **Stage 3 — G2, calls beyond leaves.** Extend `emit_call`'s inline to
  non-leaf certified callees (guard the callee, set up the frame, enter its
  compiled code, fall back to `call_slow` on any miss) — the pattern the leaf
  path already uses at a site. Gate: the instrument's call share drops;
  `non-leaf call` moves; no row regresses.
- **Stage 4 — G3/G4/G5, literals, iteration, string building.** Each is a
  helper-per-op sequence today; each gets an inline path or a fused step.
- **Stage 5 — G6/G8, cells and feedback.** Capacity for the colliding cells,
  and a per-site feedback record so the fast paths above are a mechanism
  rather than a set of bespoke arms.

Each stage: the instrument re-run, the engine gates (fmt, clippy
`-D warnings`, workspace tests, both `--no-default-features` checks), the
corpora (test262 `all` + `intl402`, the eight wasm suites, the JS-API sweep,
the corpus parity run), and the `../.agents/skills/slag-jit` traps respected
(the four-file helper mirror, the sealed-block rules, the dispatch sentinels
and pending-error ABI, `bump_leaf_epoch`, the frame-slot/leaf exclusions, the
GC-root discipline, the compile-size budget).

## 6. Out of scope, and the traps

- **Not a TurboFan rewrite.** The plan works inside the per-`Step` lowering
  and the helper ABI: inline fast paths with guards, wider use of the register
  path, and a feedback record. A typed SSA IR is a different plan and would
  need its own justification.
- **Not more coverage.** `.notes/non-leaf-jit.md` is done; do not reopen it
  without a census that says a new shape refuses.
- **Not row-only slices.** Four slices moved rows 17-46% and moved nothing
  else; a stage is judged by the instrument and by real-world wall time.
- **Risks, from the `slag-jit` skill** (each has cost real debugging time):
  the four-file helper mirror; helper signatures must match the machine code's
  sig exactly; the pending-error ABI and the dispatch sentinels; the sealed
  block/back-edge rules; `bump_leaf_epoch` for any helper that disturbs leaf
  eligibility; a new helper that writes a frame slot must be leaf-excluded;
  deferred-borrow/stack discipline under `--gc-stress`; and the per-profile
  compile-size budget, which inline paths grow.
- **The trap this plan exists because of**: "it compiled" is not evidence, and
  neither is a ratio from a suite that covers only the inlined shapes.

## 7. Definition of done

- The helper-share instrument exists and the gap order is measured.
- The G1-G6 surfaces show a collapsed helper share on real-world JS, with
  `--jit-bench` not regressing and the corpus parity run at 0 mismatches.
- A written statement of what remains between the compiled tier and a V8-class
  JIT, if the surfaces are done and the gap is still large — the honesty
  `non-leaf-jit.md` §9 closed with.

## 8. Status log

One line per landed stage, newest last.

- (plan written 2026-09-28; no code changed)
- **Stage 0, the code audit (2026-09-28).** The architecture read out of the
  emitter: per-`Step` lowering, 131 helpers, an inlined subset (numeric
  fast paths, the fused loop counter, register-op bodies, named member cells,
  global cells, dense append, typed-array store/length, leaf calls), and
  structural helper surfaces at computed-key reads, non-leaf calls, literals,
  iteration and string building. §3 is the resulting gap list; the earlier
  draft's "--jit-bench says the JIT is fine" conclusion is withdrawn as
  measuring only the inlined shapes. No code changed.
