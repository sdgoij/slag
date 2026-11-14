# Non-leaf JIT: the coverage plan

The plan of record for widening the JIT's coverage past the shapes it handles
today, named in `.notes/embedding.md` §9 and sized from a measurement of the
one workload that exposes the gap. Written before any of it is built.

Status: two milestones landed and measured — (1) `Step::Unary` (§5.1), (2) the
compile size cap (§6), plus `Step::TypeofIdent` (§6) which took the census's
refusals to 0. **The plan's expectation for the third lever was measured and
refuted**: the uncertified bodies are ~1% of body entries, not the larger wall
(§6), and with coverage fixed the JIT is still 14% net-negative on the tsc probe
— so the next part is the full-check JIT A/B, not more coverage. Everything else
below is plan. The census numbers in §3/§4 were measured on 2026-09-26 against a
debug `deno.exe` built from this tree; §5.1's and §6's after-measurements are
2026-09-27 on the same checkout, A/B'd against the committed binaries.

## 1. Why this exists

The bridge installs the JIT for every isolate it makes, so a `deno_core` host
now compiles. Two measurements of that host's own workload say the JIT is
worth ~nothing there, and they are what this plan is for:

| workload | JIT on | JIT off |
|---|---|---|
| 50M-iteration user loop (`f(n) { let s = 0; for (…) s += i; return s }`) | **4.6 s** | 11.2 s |
| the JS tsc type-check (`deno test --reload` on a one-line test file) | **> 900 s** | 14m20s |

The loop is a *leaf* — a body with no calls — and the JIT compiles it and wins
2.4×. tsc is calls end to end and the JIT changes nothing. The first draft of
the record said the gate was certification; a census said otherwise, and the
second draft said the gate was leaves; the censuses below say otherwise again.
Both corrections are kept in §9's history rather than tidied away, because the
sequence is the reason this plan starts with a measurement rather than a
feature.

## 2. What exists (the ABI a body meets)

- **Four consult sites**, all through `runtime::jit::lookup_info`
  (`crates/runtime/src/jit.rs`): `run_jit_body` (the general path, from
  `run_compiled_body`), `run_jit_resume` (suspension), `run_jit_leaf` via
  `try_jit_leaf`, and `leaf_call_probe`. `run_compiled_body`
  (`crates/runtime/src/function.rs:2629`) takes the JIT path for **any**
  certified body — its own comment says so: "the general path runs certified
  bodies that may contain calls (leaf bodies never do — `steps_are_leaf`
  excludes every call step)". So non-leaf bodies are already offered; what
  stops them is what the emitter can lower.
- **Two gates before a body runs compiled**: the *scope* gate
  (`ir.scope.is_some()`, `analyze_scope` in `crates/runtime/src/ir.rs`, which
  certified **14,444** bodies against 1,553 refusals in 150 s of the check) and
  the *emit_step* gate (a body with a scope bails when any step has no arm).
  The second is this plan's target. Its failure is sticky: `lookup_info` writes
  `jit_info = 1` ("known non-compilable"), so a body that bailed once is never
  re-consulted — see trap §8.
- **The threshold gate** (Cut 69, `JIT_COMPILE_THRESHOLD = 16`): a body with a
  loop compiles on its first consult, a straight-line body on its 17th.
  Measured in the same 150 s: **493** straight-line bodies spent on the
  interpreter by this gate.
- **The helper ABI**: compiled code calls Rust helpers through a vtable
  (`JIT_SLOW_PATHS` in `runtime/src/jit.rs`, mirrored in
  `crates/jit/src/{helpers,lib}.rs` and `compiler.rs`'s `emit_step` arms — the
  four-file mirror). A helper that errors calls `slow_error` (sets
  `ctx.pending`/`ctx.error`); `call_slow` checks the pending byte after *every*
  helper and routes to `error`/`dispatch_error`. Control helpers return a step
  index or `DISPATCH_PROPAGATE`/`DISPATCH_DONE`. This is the machinery that
  makes calls-in-compiled-code cheap in the *plan* below: a JS call does not
  need compiled-to-compiled inlining to start with — it needs the emitter arm
  plus the existing helper.
- **The leaf paths are separate** and stay so: `try_jit_leaf` runs a leaf's
  body in place on the caller's `Vm` (Cut 25/28/30/35), with a probe cache and
  its own revalidation rules. Non-leaf work must not disturb them.

## 3. The census (the sizing)

Command, kept here so it can be re-run after each slice:

```sh
JIT_DUMP_CLIF=1 deno.exe test --reload <a one-line test file> 2>&1 | grep 'jit bail'
```

`JIT_DUMP_CLIF` is a pre-existing debug switch in `JitEngine::compile`
(`crates/jit/src/compiler.rs:91`) that prints `jit bail: {reason:?} (N steps)`
for every body the emitter refuses. Over 150 s of the check:

| bail reason | bodies |
|---|---|
| `Step("Unary")` | **106** |
| `Step("unsupported step")` (the catch-all name — §18 of the JIT skill says this is `Construct`, i.e. `new`) | 2 |
| `Step("Destructure")` | 2 |
| bodies that compiled | 7 |
| straight-line bodies the threshold kept on the interpreter | 493 |

Three facts to carry into the plan:

1. **One step kind dominates the refusals**: 106 of 110 bailing bodies contain a
   `Unary` step the emitter has no arm for. That is the cheapest measured win
   available and it is first in the order below.
2. **The offer volume itself is small.** Roughly 610 bodies reached the consult
   path in 150 s of a workload that runs for fourteen minutes. Lowering `Unary`
   therefore helps ~600 bodies, not the wall clock — which is why Milestone 0
   below is a measurement rather than an emit arm.
3. **Neither certification nor leaf-ness is the gate.** 90 % of bodies certify
   and non-leaf certified bodies are offered, so widening `analyze_scope`
   (parameter defaults are 44 % of the *uncertified* minority) buys nothing
   until the offered bodies compile. The census's most useful negative result.

## 4. Milestone 0 — the offer-volume question, and its result

The question it had to answer before any emit arm: why do ~610 bodies reach
`lookup_info` in 150 s of a 14-minute workload? Until that was answered, emit
coverage could not be shown to matter, and the plan risked optimising the wrong
600 bodies.

Measured (a per-body entry profile over 180 s of the check: an env-gated probe
at the two body-entry paths, `run_compiled_body` and `Vm::run_inline_leaf`,
printing a top-15 snapshot every million entries; probe removed again).

| | |
|---|---|
| body entries in 180 s | ~3,000,000 |
| distinct bodies entered | **1,021** |
| of the top 15 by entries | 12 compiled (`jit_info` is a code pointer), **3 bailed** (`jit_info == 1`) |

The top of the profile is tsc's lexer and parser: bodies whose env-chain reads
are `text`/`codePointAt`, `isASCIILetter`/`isDigit`/`isWordCharacter`/
`isUnicodeIdentifierPart`, `pos`/`end`/`codePointUnchecked`/`isShebangTrivia`,
`setTextRangePos`/`setTextRangeEnd`, `getTransformFlagsSubtreeExclusions`,
`isKeyword`. The highest-entry body of all is the **2,212-step `loop=true`
scanner** — and it is one of the three that bailed.

Three answers, and the first is what the plan needed:

1. **The offered set is the hot set**, so emit coverage is the right axis after
   all. ~600-1,000 distinct bodies out of tsc's ~16,000 take the entries,
   because the 14 minutes are a *loop over 9 MB of text* rather than a wide call
   graph. There is no second hot set hiding outside the JIT's view.
2. **The hottest bailed body is the scanner's own loop, and its reason is
   measured rather than inferred**: the census log's own step count makes the
   correlation exact — the same run that profiled that body also printed
   `jit bail: Step("Unary") (2212 steps)`. So Milestone 1 un-bails the single
   hottest loop in the workload, which is why it is first.
3. **The wall clock is nevertheless not in body entries.** Three million
   entries in 180 s is nothing, so the time is spent *inside* those bodies —
   the scanner's own loop — and in the builtins it calls per character
   (`codePointAt` is Rust). That is why §7 keeps builtin-bound work out of
   scope and why §9 does not promise wall-clock movement from an emit slice
   alone: a compiled scanner loop can only be as fast as the helpers it calls.

## 5. Milestone 1 — lower `Step::Unary`

The measured target: 106 of 110 bailing bodies, and — from Milestone 0 — the
**2,212-step scanner loop** among them, which is the highest-entry body in the
whole workload (`jit bail: Step("Unary") (2212 steps)`). What it needs, per the
four-file mirror (§1 of the JIT skill) and §18's warning that a handled step
must also name itself:

1. `emit_step` arms for the unary kinds the IR actually carries. The coercing
   kinds (`-x`, `+x`, `~x`) can call the interpreter's existing unary slow path
   the way `binary_slow` is called for arithmetic that is not two numbers; the
   non-coercing ones (`!x`, `typeof`, `void`) are a compare/constant each.
   Which kinds exist, and whether a `unary_slow`-style helper already exists,
   is the first thing to read (`crates/runtime/src/ir.rs`'s `Step` and
   `runtime/src/jit.rs`'s helper list).
2. `step_name`, `max_stack_usage` and (if it has jump targets) `step_targets`
   entries — the last is the `body_has_loop` mirror whose skew only
   misclassifies the loop heuristic, never semantics.
3. Any new helper through all four files, with the signature matching the
   imported sig exactly.
4. `delete` is a *different* step (`DeleteProperty`-shaped) and is not in this
   slice even though it is a unary operator in the source language — the bail
   name says `Unary`, and the slice is what the census named.

Verification (the same for every milestone, and stated once):

- A focused `crates/jit` test that compiles a certified body containing each
  lowered kind and asserts parity against the interpreter (the existing
  `compile_and_run_*` tests are the pattern).
- The differential battery §7 of the JIT skill asks for: jit, `--jitless` and
  `--gc-stress` on the shapes the slice touches, plus node as the oracle where
  a semantic question arises.
- `cargo test --locked --workspace`, `cargo clippy --locked --workspace
  --all-targets -- -D warnings`, `runtime --no-default-features`, and the
  test262/wasm sweeps — `crates/jit` and `crates/runtime` are in every
  runner's graph, so the corpora are owed on any emit change.
- **The measurement, not the compile**: re-run the census (`JIT_DUMP_CLIF`)
  and expect the `Unary` bails to reach 0 and the compiled count to rise; then
  re-measure the two rows of §1. A slice that compiles more bodies and moves
  no wall clock is the outcome §18's last paragraph warns about ("compiling a
  body is not the same as making it faster"), and it is a result worth
  recording rather than hiding.

### 5.1 The result (measured 2026-09-27)

Landed. The census is exactly as this section predicted: the `Step("Unary")`
refusals go **111 → 0** and the compiled count **439 → 567**, with **no new
refusal reason** — so the next wall is the census's other two rows, `Construct`
(the unnamed catch-all) and `Destructure`. The vehicle is the file-based tsc
probe (`deno run --allow-read target/tsprobe.js`), A/B'd on one checkout: the
pre-slice `deno.exe` kept as `target/deno-before.exe`, then `cargo build -p deno`
from inside `deno/` (2m29s with `CARGO_INCREMENTAL=0`).

**The wall clock says the next milestone is not `Construct`.** The same probe is
**31% slower** after the slice: 132.99 s before (window 14994 + ns 57908 +
es5 56977 + program 553 + diagnostics 2561 ms) against 173.87 s after (23619 +
56030 + 68368 + 9192 + 16660), with the `A:` tsc-load stage as the control at
16448 vs 16547 ms; the same direction with `JIT_DUMP_CLIF=1` on
(154.9 s → 204.2 s). The cause is measured, not inferred: a temporary `JIT_TIME`
probe inside `JitEngine::compile` (removed again, the tree verified byte-identical
above it) totals **72.4 s of Cranelift compile** over the after run's 567 bodies —
six bodies over a second (17.9 s between them; the 1,707-, 2,277- and 2,212-step
scanners at 4.44 s, 4.34 s and 3.78 s) and 170 over 100 ms (58.5 s of the total).
One script's pass over tsc cannot repay a four-second compile, and this is a
**debug** build where `cranelift-codegen` is unoptimized.

The constant the measurement indicts is Cut 69's, and §7 said not to touch it
without one: a body with a loop bypasses the threshold entirely and compiles on
its first consult, however large — **so the threshold is too permissive on the
loop side**, not too strict on the straight-line side §7 was worried about.

Row 1 of §1 is unchanged, as it must be: `hot.js` (a leaf loop whose body has no
unary step) runs 1.97–2.05 s on the before binary and 1.90–1.99 s on the after
one. The differential battery (jit / `--jitless` / `--gc-stress` / node) is
byte-identical on `target/unary_probe.js`, and the gates and corpora are
unchanged — `.notes/embedding.md` §7 has the full record.

## 6. Milestone 2 and after (ordered by the census, re-taken each time)

- **First, the compile-cost gate — created by Milestone 1's measurement (§5.1). Landed.**
  A body with a loop compiles on its first consult regardless of size, and the A/B
  measured what that costs: **52.2 s** of Cranelift compile over 567 bodies (the 125
  over the cap cost 36.6 s of it), against a tsc probe that the `Unary` slice had
  made **15 s slower** than before it. So the threshold gained its second axis:
  `JIT_MAX_COMPILE_STEPS = 128` in `crates/runtime/src/jit.rs`, checked in
  `lookup_info` before the threshold, refuses the body with the same sticky `1` an
  unsupported step writes, and prints `jit skip: body too large (N steps)` under
  `JIT_DUMP_CLIF`. A body with a self-tail-call is exempt (its iterations are
  unbounded within one call — the tests caught the first cut sending the
  65-argument vector self-jump of Cut 51/52 to the interpreter). Measured on the
  file-based tsc probe, two samples each: **ungated 151.7 s, pre-`Unary` 136.4 s,
  gated 129.7 s** (shipped binary, min-of-2: 155.8 / 140.5 / 134.7 s). The census
  after the gate: 0 `Unary` bails, **122 skips**, 496 compiled, **one refusal left —
  the unnamed catch-all (`Construct`)**. `.notes/embedding.md` §7 has the histogram
  and the sweep table. Alternatives considered and rejected: an inline number fast
  path for `-x`/`+x`/`~x` (worth having, but it cannot fix a cost that is compile
  time), and a `SLAG_NO_JIT`-style switch (measures, does not fix).

- **`Step::TypeofIdent` — the real remaining refusal, not `Construct` — landed (Cut 93).**
  This bullet previously named `Step::Construct` on the `slag-jit` skill's §18,
  and a probe showed the skill was stale: `Construct` has its `emit_step` arm
  (`Helper::Construct`), its `step_name` entry and its `max_stack_usage` entry,
  so a body containing `new` compiles. A temporary print naming the refusing
  step's `Debug` in `emit_step`'s catch-all answered `TypeofIdent { name: 8116 }`
  — a `typeof x` over a `BindingLoc::Env` name — and enumerating the `Step` enum
  against `crates/jit/src/compiler.rs` confirmed it was the only uncovered
  variant a certified body can carry (31 are absent from the emitter, 30 of them
  unreachable from a certified body). Lowered as a helper mirroring the
  interpreter's own arm (`typeof_ident`), with `step_name` and `max_stack_usage`
  entries added. **Measured: the census's refusals reach 0** (0 `Unary`, 0
  catch-all), 122 `body too large` skips, 497 compiled. `.agents/skills/slag-jit/
  SKILL.md` §18 is corrected in the same change.

- **`Step::Construct`** (already lowered — kept here only so the record of why it
  was *planned* is not lost, and because the skill's stale claim is worth not
  repeating). A body containing `new` compiles: `emit_step`'s `Construct` arm
  calls `Helper::Construct`, which runs the interpreter's construct machinery (the
  construct-inline leaf cache / general path). It was never the gap.
- **`Step::Destructure`** (2 bodies in the pre-gate census; in the post-gate
  census the same bodies are *skipped as too large* rather than refused, so the
  emitter's view of them is no longer visible and a raised cap would show whether
  the arm is still missing). The JIT skill's §10 has the
  rules (flat binds, the for-head trap, the close gates).
- **Then re-census — and with the emitter's refusals at 0 for this workload, the
  two levers are named rather than left implicit.** (1) The **122 bodies the size
  cap refuses**: they are off the JIT entirely, so the questions are whether the
  cap is set right (it was sized on a debug build, where Cranelift is ~10× more
  expensive — a release-side measurement is the better tuning input) and whether
  any of them is hot enough to earn a compile through a larger consult count
  instead of an absolute cap. (2) The **~1,553 uncertified bodies** of §3.
  **Measured 2026-09-27: (2) is not the lever.** Uncertified bodies take **0.31%**
  of body entries on the tsc probe (8,297 of 2,719,876, over 1,718 refused bodies)
  and **1.2-2.5%** on the full forced `deno test --reload` check (at least
  200,000 of 16,285,554); certifying every one of them is worth about **0.8%**,
  even though certification as a whole is worth **29%** of the probe (197.8 s with
  `SLAG_NO_CERT` against 140.6 s) — the value is in the 98% of entries that
  already certify. The two hot refusal sites are param rest/default (94% of the
  probe's uncertified entries; 54% of the full check's) and `scan.stmts` (the
  body-construct bucket; the rest), so the *tractable* half is param rest/default,
  which needs spec 10.2.11's separate Parameter Environment — a risky semantic
  slice for ~0.6 s of a 14-minute check. **What replaces it as the next part:**
  with coverage fixed (0 refusals) the JIT is still **14% net-negative** on the
  tsc probe (JIT off 125.3 s against JIT on 142.5 s, two samples each), so the
  question is no longer coverage but whether the compile pays over the window it
  is measured in — and the probe's window is 2.5 minutes against the check's 14,
  so **the next measurement is the same A/B on the full check** (two 14-minute
  runs), which decides whether the JIT is worth its compile on the workload this
  plan exists to serve.

## 7. Out of scope, and why

- **Builtin-bound work.** The 14 minutes are dominated by loading and parsing
  9 MB of `.d.ts`, which is string and `Map`/`Set` work inside Rust builtins.
  No JIT arm changes that; the file-based tsc probe measures exactly this and
  is flat against the JIT (2m27s vs 2m5s). A genuine fix on that axis is
  engine throughput in `crux`'s string/collection code, a different plan.
- **Certification widening.** §3.3 argued it buys nothing *until* the offered
  bodies compile; measured after they do, it buys **~0.8%** (uncertified bodies
  are 0.31% of body entries on the tsc probe and 1.2-2.5% on the full check,
  against a 29% value for certification as a whole). Out of scope on the
  numbers, not on principle — a future part may revisit it if a workload's
  uncertified share is large (a codebase heavy in default parameters, say).
- **Removing the leaf paths / unifying them with the general path.** They exist
  for a reason (Cut 25-35) and are the JIT's current wins.
- **The threshold's value.** 493 straight-line bodies sit on the interpreter
  because of it. That is by design (compile cost is a frame's budget for a
  small body) and the plan does not touch it without a measurement that says
  the constant is wrong. **The measurement arrived with Milestone 1 (§5.1), and
  it points the other way:** the straight-line side is not the problem — the
  loop side is, because a loop body never consults the counter at all. The
  gate is §6's first milestone now.

## 8. Traps (the ones this work will actually hit)

- **A bailed body is sticky.** `jit_info = 1` is never cleared except by
  eviction, so a body that bailed before a slice landed will *not* be
  recompiled in the same process. Every measurement after a slice needs a
  fresh process, and a test that asserts "this body compiled" must build the
  body after the change (a stale `jit_info` in a long-lived process is a false
  negative).
- **The four-file mirror.** A helper field added to one of the four files and
  not the others fails to compile — except a *signature* mismatch, which
  surfaces as a Cranelift verifier error at compile time. Both are checked by
  the plan's gates.
- **Step-index helpers read the step, not a marshalled payload**, and
  fixup-patched fields are only correct read from the running body (the
  `ForOfBegin` boundary span is the precedent).
- **`bump_leaf_epoch`**: a helper that can mutate the Vm stacks or the realm
  count must invalidate cached leaf-call verdicts, or a compiled caller inlines
  a stale verdict.
- **The sealed-block rules** for any new jump target: a block that receives a
  jump from a later step must be in `back_targets`, and a `cond_jump`'s
  fall-through must be `index + 1`.
- **Deferred `borrow`/stack discipline**: helpers must not hold a `RefCell`
  borrow or a heap-allocating local across a safepoint (the
  `--gc-stress`/A2 verifier runs in every debug test, which is the net).

## 9. Definition of done

Per milestone: the census re-taken (the named bails at 0), the engine's gates
and the corpora green, the focused test plus the differential battery, and the
two rows of §1 re-measured with the same-binary A/B the JIT wiring was measured
with. **Milestone 1 met all four parts (§5.1) and its wall-clock row moved the
wrong way — the compile-cost gate now leads §6 and is landed with its own
measurement — so `Construct` is the next milestone after all.**

For the plan as a whole: the forced `deno test` check's wall time moving
off 14m20s, or a written statement of what the offered bodies' shape is that
makes the JIT the wrong tool for it — either is a result; the plan does not
promise the first.
