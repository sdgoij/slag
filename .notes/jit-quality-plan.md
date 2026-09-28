# JIT execution quality: the inline-coverage plan

The plan of record for making a compiled body fast, as opposed to compilable.
`.notes/non-leaf-jit.md` is the coverage plan and reached its own definition of
done (0 emitter refusals) — this plan exists because coverage turned out not to
be the thing, and because the architecture below was read out of the code
rather than inferred from timings.

Status: **Stages 0-5** are largely done (2026-09-28): the code audit, the instrument, G9 (the integer operators inline), G2 (the leaf-call refusal cache), G12 (`this`-slot leaves inline), G13 (the env-leaf lane), G14 (a cached frame descriptor), G15 (a per-callee call record), and G1's read side (the inline dense-array element read). Stage 5's remaining items — the chain read, `switch`, and the member-write path — are recorded in §5 as **closed by measurement** (already measured and reverted, or already cheaper than an inline path can beat), not as open work. The JIT is a per-`Step` lowering with hand-written
inline fast paths for a named subset and `call_slow` into a 131-entry Rust
helper table for everything else — **compiled dispatch with an inlined core**,
not a typed-IR compiler. §3 is the gap inventory, ordered by measured helper
traffic (the Stage 1 run, §4) and annotated with what has landed; §5 is the
stage order. The instrument is gated off by default.

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
- **Where that subset actually ends (measured, §3).** Four boundaries this
  bullet did not know about, two now closed: a named read inlines **only for an
  own data property** — any prototype-chain link, which is every method read,
  takes `call_slow(GetMemberName)` (§3 G10); the leaf-call inline path was
  **wired but bypassed**, because a *transient* refusal was cached as permanent
  (§3 G2, closed in Stage 3); the integer operators did not inline at all until
  Stage 2 closed that (§3 G9 — `&`, `|`, `^`, `<<`, `>>`, `>>>` now lower to a
  guarded `i32` op); and a leaf whose body uses `this` was refused outright,
  which is every method — Stage 3 closed that too, and priced the lane it is on
  (§3 G12; the alias `call_slow` ≈ 48-66 ns against ≈ 6-39 ns inlined).
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

**Measured first (Stage 1, 2026-09-28): helper invocations over the 77-workload
corpus, one process per row** so a row's counters belong to it, summed:

| helper | invocations | rows | surface | first action |
|---|---|---|---|---|
| `BinarySlow` | 255.8M | 50 | integer operators (bitwise/shift); `+`/`-`/`*` inline | **G9 — landed (Stage 2)** |
| `CallSlow` | 102.2M | 50 | every call not inlined by the leaf path | **G2 — landed (Stage 3)** |
| `SwitchTest` | 91.9M | 1 | one per `case`, per execution | **new (G11)** |
| `GetMemberName` | 90.4M | 54 | prototype-chain reads (every method read) | **new (G10)** |
| `GetMemberComputed` | 73.4M | 12 | computed read `a[i]`; the write side has two inline paths | G1 |
| `SetMemberSlot` | 70.0M | 3 | member writes off the in-place path (`this.x += n`, churn) | G1-adjacent |
| `LeafCallProbe` | 33.7M | 50 | the probe re-run on epoch churn — plus the dead-path tax of G2 | G2 |
| `LoadContext` | 30.3M | 4 | closure/context slot loads | — |
| `ForOfNextBindLocal` | 21.0M | 1 | `for-of` step | G4 |
| `SwitchDisc` | 21.0M | 1 | `switch` selector | G11 |
| `BreakControl` | 21.0M | 1 | `break` inside a `switch` | G11 |
| `ObjectFast` | 16.8M | 16 | object literal | G3 |
| `TypedArrayLength` | 15.7M | 16 | typed-array `length` | — |
| `ForInNext` | 6.3M | 1 | `for-in` step | G4 |
| `ConcatStrings` | 4.5M | 3 | string building | G5 |

Caveat, stated because it moves the ranking: the 36 `opcost` rows are generated
with `k = i & MASK` and `s = (s + x) | 0` idioms, so they over-weight
`BinarySlow`. The bitwise gap is real (verified with dedicated shapes, below),
but its corpus share is inflated; the non-opcost families put calls, method
reads and computed reads at the top instead.

The list, in the measured order:

- **G9 — integer operators (landed, Stage 2).** They used to lower to
  `call_slow(BinarySlow)`, whose helper re-does the full `apply_binary`
  (ToNumeric + `ToInt32`), while `+`/`-`/`*` on numbers inlined. Isolated
  1M-iteration shapes before: `s = s + i * 2` 0.81 ms with **0** helper calls,
  `s = s + (i << 1)` 12.0 ms with 7.0M `BinarySlow`, `k = i & MASK; s = (s + k)
  | 0` 23.5 ms with 14.0M. Now the six operators inline as a guarded `i32` op
  (`emit_int_binary`): both operands truncate to `i32` through the f64→i64
  conversion (`cvttsd2si`; the `ireduce` is `ToInt32`'s modulo-2^32 step, and
  cranelift's documented shift masking is the spec's `& 0x1F`), behind a range
  guard (`|x| < 2^63`) that falls back to `BinarySlow` where saturation would
  disagree with the spec's wrap-around (`NaN`, the infinities, `1e300`, `2^63`).
  After: **`<<` 12.0 → 2.74 ms (−77%), `&`/`|0` 23.5 → 6.87 ms (−71%)**, both at
  0 `BinarySlow` on in-range operands; the corpus's `mean-jitGap` 77.5 → 68.4 and
  `--jit-bench`'s `typed-array write` 0.46 → 0.21 (the row whose body is
  `ta[k] = k & 255`).
- **G10 — named reads inline only for an own data property.** The read cell
  covers `o.x` when `x` is an own data property; any prototype-chain link (a
  method read, an inherited property, an accessor) falls back to
  `call_slow(GetMemberName)`. 1M-iteration reads: own data 2.5 ms, 1
  `GetMemberName`; inherited data 18.3 ms, 7.0M; `a.push` 18.6 ms, 7.0M — **~7×**.
  Every method call therefore pays a helper on its callee read, which is why
  `GetMemberName ≈ CallSlow` in the builtin-bound rows.
- **G11 — `switch` is a helper chain.** `SwitchDisc` once, then one
  `call_slow(SwitchTest)` per `case` with a dispatch per test, plus
  `BreakControl` per `break` (`compiler.rs:6297`, `:6308`, `:5946`).
  `switch_dispatch.js` measures 91.9M `SwitchTest` + 21M `SwitchDisc` + 21M
  `BreakControl` over 3M iterations — ~30 helper calls per iteration for a shape
  the interpreter handles as a jump table.
- **G2 — the leaf-call inline path (landed, Stage 3).** The refusal that
  matters is `no-compiled-code`: the callee is a straight-line body, so Cut 69
  defers its compile until `JIT_COMPILE_THRESHOLD` (16) consults, while
  `leaf_call_probe` recorded *every* refusal as a stable zero verdict that Cut
  68's restamp then reused across epoch bumps. A refusal that is only a
  deferral must not be cached as permanent, so the probe now clears the cache
  identity when `jit_info` is not the sticky `1` (a genuine over-cap or emitter
  refusal still caches, so a permanently ineligible callee is not re-probed
  every visit). Measured at the fixed site (`direct_leaf`): `CallSlow`
  2,000,000 → **8**, `LeafCallProbe` 7 → 15, the site inlining after its
  tier-up window; corpus `mean-jitGap` 68.4 → **65.2**.

  **The Stage 1 claim here — "100% of calls take `CallSlow`", the inline path
  "effectively dead" — was an over-read of the aggregate counter and is
  withdrawn.** In `bench_once` a call site's *first* invocation pays the
  refusals while later invocations already inline: the leaf's body compiles
  after 16 consults (`try_leaf_call`'s lane consults `lookup_info` on every
  call), and the harness's two warmups exclude that first invocation from the
  timing. So the 2,000,000-call aggregate was one un-measured pass, and the
  defect is narrower than stated: it costs a site's first invocation, which is
  the whole program for a single-entry long-running loop. Priced by measuring
  both lanes in one build (a permanently refused `this`-slot callee against an
  inlinable one, same body, 3M iterations): **`CallSlow` ≈ 48 ns/call against
  ≈ 6.4 ns inlined, ~7.5×.**
- **G12 — a leaf that reads `this` (landed, Stage 3).** The probe refused
  `scope.this_slot.is_some()` outright — "the machine code cannot bind `this`"
  — so every method call stayed on `call_slow`, and it is why `this` reads lower
  to `LoadLocal { this_slot }` (a frame read, leaf-eligible) rather than to
  `Step::ThisValue` (leaf-excluded). The probe now takes the call's unbound
  receiver, applies `OrdinaryCallBindThis` as the interpreter's own certified
  path does (strict: as-is; sloppy: an object as-is, a nullish one → the realm's
  global object, a *primitive* one boxes — and boxing allocates and can throw,
  so only that case still refuses), and fills the frame's `this` slot. The
  helper signature gained the receiver, so the four-file mirror was paid.
  Measured on `calls/method_call.js` (an object receiver): `CallSlow`
  14,000,000 → **8**, the row 132.7 → **77.9 ms (−41%)**. Verified
  behaviourally: a differential over method receivers, sloppy plain calls
  (→ global), strict plain calls (→ undefined), boxed and unboxed primitives
  and a `this`-capturing closure is identical between the two modes. Two
  sub-cases still refuse and both are pre-existing: a body that reads `this`
  *and* an identifier (`LoadIdent` is leaf-excluded, so such a body was never
  leaf-certified — G13's territory), and a strict body's plain-call
  `return this`, which refuses upstream of the probe.
- **G13 — the environment-using leaf (landed, Stage 4).** The earlier text
  grouped `closure_capture`, `recursive_fib`, `generator_loop` and
  `nested_read` as "bodies that are not leaf-certified", attributed to a
  `leaf_lookup` miss. Measuring all four (2M-call harness, release) split them
  by root cause, and only one is the leaf lane:

  | row | jit | jitless | root cause |
  |---|---|---|---|
  | `calls/closure_capture` | 82 ms | 139 ms | an env leaf, but its site is **polymorphic** (64 closures) — see G15 |
  | `globals/nested_read` | 2.6 ms | 17.6 ms | already the compiled `LoadIdent` cell probe; one `call_slow` per bench |
  | `calls/recursive_fib` | 310 ms | 474 ms | a re-entrant callee — a leaf contains no call, so this row has no leaf lane |
  | `control/generator_loop` | 90 ms | 121 ms | generator resume machinery, not the call lane |

  The tractable slice is the ENV leaf. `closure_capture`'s inner body reads
  captures, which lower to `Step::LoadContextSlot` — leaf-safe (`steps_are_leaf`
  does not exclude it, and the interpreter's own `fast_call_core` leaf gate has
  no env condition; only `run_jit_leaf`'s fresh-ctx path and the JIT probe's
  `ir.leaf_uses_env` refusal did). So a body reading a captured binding was a
  certified leaf the compiled code could never call — which is the most common
  inner function there is.

  **The machine code cannot run it in-frame**: the
  `body_context`/`lexical_env` swap has to span the call including its error
  exit, and the leaf runs on the CALLER's ctx, so both must be put back before
  the caller resumes. So the probe records the verdict with `uses_env` set (and
  still refuses, so the site's first call goes to `call_slow`), and the compiled
  hit path gained an env lane: `leaf_call_fill` rebuilds the frame (the same
  helper and room check the in-frame lane uses), then `leaf_call_env`
  re-derives the callee's environment, swaps it in, calls the compiled entry on
  the caller's ctx and buffer, and restores — mirroring `run_jit_leaf`'s env
  handling and tail in the same order. A frame too wide for the record's TDZ
  mask records `entry = 0` instead, keeping the site on `call_slow` rather than
  filling from a truncated mask.

  Measured on a MONOMORPHIC env leaf (`function bench(){ var base = 1; function
  inner(x){ return base + x; } … }`, 2M calls): **92 → 40.5 ms (−56%)**, with
  `CallSlow` at the site 2,000,000 → **15**, against 88 ms interpreted.
  `closure_capture` itself cannot show it — its site is polymorphic (G15).
- **G15 — a megamorphic call site holds one record per callee (landed,
  Stage 4).** The call record holds ONE callee identity, and its slot was a
  pure function of the SITE, so a site that sees N callees re-probed on nearly
  every visit. The measured curve (env leaf, 1M calls per bench) was the shape
  of it: **mono 3.0 ns/call, 2 callees 10 ns/call, 64 callees ~13 ns/call**,
  and `calls/closure_capture` (64 closures through `fns[i & 63](i)`) showed
  `LeafCallProbe`/`CallSlow` ~7,000,000 for 1,000,000 iterations — every visit
  probed, then fell to `call_slow`, and the env lane never ran. A floor build
  whose probe returned immediately priced the probe's BODY at only ~1.4-3.5
  ns/call, so the cost was the verdict never holding, not the probe's work.

  The record is now keyed by the CALLEE and lives on the `Agent`
  (`Agent::leaf_records`), addressed through a base pointer in `JitCallContext`
  so a run — including the hot leaf path — pays no setup: an inline table made
  every run memset it, and a 64-entry ctx table took `calls/recursive_fib`
  329 → 732 ms. The verdict (entry + frame descriptor) is a pure function of the
  callee's compiled body, so one record serves every site that calls it. The
  slot is `runtime::jit::leaf_record_slot(callee)`, a golden-ratio multiply
  taking the high bits (a low-bit fold collapsed consecutive box allocations
  onto a handful of slots), mirrored by the compiler's `emit_leaf_record_slot`
  and pinned by `the_emitter_and_runtime_leaf_record_slots_agree`.

  Because a record's `entry` outlives a run, it also carries the code
  generation (`LeafCallRecord::code_gen` against `JitCallContext::leaf_gen`): a
  run bumps `Agent::leaf_gen` before its first `lookup_info`, whose compile may
  evict and free the code a previous run's record named, so a stale record
  re-probes rather than jumping to a freed entry (nested runs share the
  generation — no eviction can happen in flight). A sweeping collection still
  clears the table, since a recycled box address could match a record's
  identity.

  Measured: `calls/closure_capture` **88.8 → 50.9 ms**, with
  `LeafCallProbe`/`CallSlow` down from 7,000,000 to ~900 and
  `leaf_call_fill`/`leaf_call_env` at 7,000,000 (the env lane on every visit).
  `LEAF_CACHE` (the interpreter's per-function leaf cache) was raised 16 → 256
  so the env lane's per-call env lookup hits instead of falling to the
  `ecma_functions` HashMap. `calls/construct_churn` 120 → ~103,
  `calls/recursive_fib` 329 → 278; no call row regressed.
- **G14 — a non-aliased frame rebuilt from a cached descriptor (landed,
  Stage 3).** `emit_call`'s cache-hit path inlines in-frame only an *aliased*
  frame (`frame_size == arity`, the argument region IS the frame); anything
  else — a `this` slot, or any `var`/lexical slot past the params — *did*
  branch back to the full probe, which re-ran every eligibility check, the
  `lookup_info` consult and the cache-record rewrite before rebuilding the
  frame. On the plan's shape (`function f(x){var t=1;return x+t}` at 2M calls)
  that was `LeafCallProbe` 14,000,000.
  The hit path now calls `leaf_call_fill`, which rebuilds the frame from a
  descriptor the probe cached in the call record (`this_slot`, `strict`, a
  per-slot TDZ mask and `fill_ok`) and returns the entry, or 0 when the frame
  no longer fits. Both paths fill through the one `fill_leaf_frame`, so a
  site's first visit (from the scope) and every hit (from the record) cannot
  drift.
  The descriptor was found by measurement, not inference: a floor build whose
  `leaf_call_fill` returned the cached entry right after the cache check priced
  the indirect call at ~0.3-1 ns and the helper's *body* — the `leaf_lookup`,
  the `Rc` clone and the scope dereference — at ~7 ns, so removing those three
  was the lever. Measured: the shape 35.4 → **21.4 ms** with `LeafCallProbe`
  14,000,000 → **30**, against an aliased control of **10.3 ms**;
  `calls/method_call` 77.9 → **58.7 ms** (that row is non-aliased, so this is
  where G12's remaining win was hiding). The residual is now the frame build
  itself (~4 ns/call fixed + ~0.7 ns/slot), so the remaining lever is a
  machine-code fill for the no-`this`/no-lexical case — recorded, not done.
  A frame wider than 64 slots sets `fill_ok = 0` and keeps the probe fallback,
  because the record's TDZ mask cannot describe it: exactness over a silent
  mis-fill.
- **G1 — computed-key access has no inline path on the read side (read side
  landed, Stage 5).** The read `a[i]` lowered to
  `call_slow(GetMemberComputed)` for every shape, while the write side already
  had inline paths; `GetMemberComputed` was 73.4M on 12 rows (`index_loop.js`
  21.0M). The compiled read now inlines the dense-Array case
  (`emit_dense_element_read`, shared by the step `GetMemberComputed` and the
  register `GetMemberComputed`/`GetMemberComputedLocal`): the receiver is a
  tagged Object whose `array_dense` cursor is live (non-null iff dense), the key
  a canonical index Number (`< 2^32-1`), and the index below `elem_len` — then
  the element is one buffer load. A hole is spec-absent and declines to the
  helper (which consults the chain), as do a spilled array, a typed array (its
  own `ObjectKind`), a string/out-of-range key, and a non-Array. Measured:
  `arrays/index_loop` 37.9 → **20.7 ms** with `GetMemberComputed` 21,000,000 →
  **0**, `objects/many_objects_read` 30.6 → **21.1 ms**, and the rows'
  JIT-vs-interpreter ratio 3.3x → **6.0x** / 3.5x → **5.1x**. Typed-array
  element reads still take the helper (a separate representation) — a
  follow-up.
- **G3/G4/G5 — literals, iteration, string building**, as before: a helper
  sequence per element/step/property, measured in the table above.
- **G6/G7/G8 — cell capacity, the register path's one shape, no feedback.**
  G8 is now load-bearing for a sharper reason: G9, G10 and G11 are each a
  *missing inline path*, and doing them one at a time is how the current
  asymmetry (an inline path for `+` but not `<<`, for an own read but not an
  inherited one) came to exist.

## 4. How this gets measured (and what has been ruled out)

- **Real-world code is the evidence.** The two synthetic instruments are
  biased (microbenchmarks: inlined shapes; the corpus: builtin-heavy). The
  measurement this plan needs is a profile of **real** JS — where the time goes
  between inline paths and helper calls — plus wall time on workloads that are
  not what the JIT was tuned on.
- **The helper-share instrument exists (Stage 1, done).** It is a compile-time
  emission in `crates/jit/src/compiler.rs::emit_raw_call` — the single funnel
  every helper call passes through — that adds a four-instruction inline
  increment of the helper's counter in `runtime::jit::JIT_HELPER_COUNTS`,
  indexed by `Helper as usize`. It is emitted **only when `JIT_HELPER_STATS` is
  set while the body is compiled**, so a default build is the same machine code
  as before and the counters cannot contaminate a measurement that is not
  asking for them; `crates/jit` asserts at compile time that `Helper::COUNT`
  fits `runtime::jit::HELPER_COUNT`. The runner hooks the CLI's `run_corpus`,
  which runs one mode over one directory in one process — point `--corpus` at a
  one-workload directory and the closing dump is that row's histogram
  (`helper <index> <count>`, resolved through `crates/jit/src/helpers.rs`'s
  enum order). `JIT_DUMP_CLIF=1` prints the full CLIF **and** the disassembly
  (the earlier note here that it prints only bail lines was wrong); the two
  together separate which helpers a body calls statically from which it calls
  in the hot loop. The stage-1 output is §3.
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

- **Stage 1 — the instrument.** **Done** (§4). Output: the measured table in
  §3, which rewrote the gap order.
- **Stage 2 — G9, the integer operators.** **Done** — §3 G9 has the lowering
  and the before/after numbers. Gate met: the isolated shapes (`<<`, `&`/`|0`)
  drop to zero `BinarySlow`, `--jit-bench` does not regress (and `typed-array
  write` improves 0.46 → 0.21), the corpus parity run stays at 0 mismatches, and
  both test262 areas reproduce baseline. Left for a later pass: the guard runs
  per operation, so a compile-time-constant operand (`| 0`, `& 255`) could
  elide half of it — the corpus says the constant-operand cases are already
  fast enough to not pay for the plumbing yet.
- **Stage 3 — G2, G12 and G14, the leaf-call path.** **All three are done**
  (§3 G2, G12 and G14, each with its own measurements and the withdrawn
  over-claim in G2). Gate: the `direct_leaf` site's `CallSlow` 2,000,000 → 8,
  the `calls/method_call` row 132.7 → 77.9 ms with its `CallSlow` 14,000,000 →
  8, the non-aliased shape 35.4 → 21.4 ms with `LeafCallProbe` 14,000,000 →
  30, `calls/method_call` → 58.7 ms, `--jit-bench` unregressed, parity at 0
  mismatches, both test262 areas at baseline. What remains of the leaf lane is
  the residual G14 records (a machine-code fill) plus the refusals that are
  not the env lane: a strict plain-call `return this`, and a body that reads an
  identifier (`LoadIdent` is `steps_are_leaf`-excluded; G13's fix covers
  captured bindings, which lower to `LoadContextSlot` — see Stage 4).
- **Stage 4 — G13, the environment-using leaf, and G15.** **Both done** (§3
  G13 and G15 have the audit table and the numbers): G13 gave
  environment-reading leaves the env lane, and G15 keyed the leaf-call record
  by the callee on the agent, so a megamorphic site holds one record per
  callee.
- **Stage 5 — G10, G11, G1, member writes.** **G1's read side is done** (§3
  G1). The others are **closed by measurement, not landed**: G10's shape-free
  inline chain probe was already measured and reverted (2026-09-01, 5-10%
  slower than the helper — `.notes/perf.md`), and warm chain reads are
  fixed-cost rather than walk-cost, so the fix is the L1c shape end-state, not
  an inline path; G11's `switch` is already 3.1x faster under the JIT than
  jitless (`switch_dispatch` 68 vs 213 ms) with the helper chain at ~6 calls
  and ~3.3 ns/iter, so inlining it cannot pay; and the member-write rows
  (`warm_store`, `compound_assign`) are ~pure `SetMemberSlot` at ~1 ns/iter.
  Recorded so a later pass does not re-open them.
- **Stage 6 — G3/G4/G5, literals, iteration, string building.** Each is a
  helper-per-op sequence today; each gets an inline path or a fused step.
- **Stage 7 — G6/G8, cells and feedback.** Capacity for the colliding cells,
  and a per-site feedback record so the inline paths above become a mechanism
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

One line per landed stage, newest last. This log is the arc's journal —
`.notes/embedding.md` is the embedding contract and does not take JIT records.

- (plan written 2026-09-28; no code changed)
- **Stage 0, the code audit (2026-09-28).** The architecture read out of the
  emitter: per-`Step` lowering, 131 helpers, an inlined subset (numeric
  fast paths, the fused loop counter, register-op bodies, named member cells,
  global cells, dense append, typed-array store/length, leaf calls), and
  structural helper surfaces at computed-key reads, non-leaf calls, literals,
  iteration and string building. §3 is the resulting gap list; the earlier
  draft's "--jit-bench says the JIT is fine" conclusion is withdrawn as
  measuring only the inlined shapes. No code changed.
- **Stage 1, the instrument (2026-09-28).** The helper-call counter landed
  (§4): a gated four-instruction increment in `emit_raw_call` (the one funnel
  every helper call passes through), the per-helper counter array in
  `runtime::jit`, a compile-time `Helper::COUNT <= HELPER_COUNT` check, and a
  dump wired into the CLI's `run_corpus`. Run over the 77-workload corpus, one
  process per row; the result is §3's table, and it rewrote the work order —
  G9/G10/G11 are new, G1 fell below calls and the read/write gaps, and G2
  became "the leaf path is reached but never fires" rather than "only leaves
  inline". Certifications on the changed tree: `cargo fmt --all -- --check`
  clean; `cargo clippy --workspace --all-targets -- -D warnings` clean;
  `cargo test --workspace` green (crux 259, jit 209, runtime 983, test262
  3324, v8 372); both `--no-default-features` checks; test262 `all`
  48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s; the
  eight wasm `run --strict` suites 64,594 checks 0 fail/0 pending and `jsapi`
  1,001/0; corpus parity 77 rows/0 mismatches; `--jit-bench` unregressed
  (`arithmetic` 0.08, `bare loop` 0.09, `function calls` 0.13, `non-leaf call`
  0.59 — all in the documented band). The instrument emits no instructions when
  `JIT_HELPER_STATS` is unset, which is why the sweeps reproduce the prior
  numbers exactly.
- **Stage 2, G9 the integer operators (2026-09-28).** The six integer operators
  (`&`, `|`, `^`, `<<`, `>>`, `>>>`) now inline for two Numbers as a guarded
  `i32` op — both operands truncate through the f64→i64 conversion with an
  `ireduce` standing in for `ToInt32`'s modulo-2^32, behind a `|x| < 2^63` guard
  that keeps `BinarySlow` for `NaN`, the infinities, `1e300` and `2^63` (where
  the saturating conversion would disagree with the spec's wrap-around). The
  shift count needs no `& 0x1F`: cranelift documents masking a shift amount to
  the operand size. Measured on isolated 1M-iteration shapes: `s + (i << 1)`
  12.0 → 2.74 ms (−77%) and `k = i & MASK; s = (s + k) | 0` 23.5 → 6.87 ms
  (−71%), both to **0 `BinarySlow`** on in-range operands (`s + i * 2` was
  already 0); the corpus `mean-jitGap` 77.5 → 68.4 over 77 rows/0 mismatches,
  and `--jit-bench`'s `typed-array write` 0.46 → 0.21 with no row regressing
  (four runs; `apply leaf call` read 0.45 once and 0.35-0.36 three times, which
  is that row's own spread). Correctness: a new
  `installed_jit_integer_operators_match_the_interpreter` test compares the
  compiled path against the interpreter over 27 edge-case operands (a fraction,
  `2^63`, `1e300`, `NaN`, the infinities, `'8'`, `true`, `null`, `undefined`,
  BigInts, `>>>`'s BigInt TypeError) with variable shift counts; removing the
  range guard makes it fail. A scratch differential over six ops × 34 edge
  values is byte-identical between the compiled and interpreted modes. Gates:
  fmt clean, clippy workspace `-D warnings` clean, `cargo test --workspace`
  5,532 passed / 0 failed, runtime `--no-default-features` 952/0, v8 `simdutf`
  380/0, `cli --no-default-features --features jit` green, test262 `all`
  48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s. No wasm
  sweep is owed, and that is checked rather than argued: `cargo tree -p
  wasmtest` does not contain `crates/jit`.
- **Stage 3, G2 the leaf-call refusal cache (2026-09-28).** `leaf_call_probe`
  cached every refusal as a stable zero verdict, and Cut 68's restamp reused it
  across epoch bumps; for a straight-line callee the refusal is only a
  *deferral* (Cut 69's compile threshold), so the site never re-consulted and
  never inlined. The probe now clears the cache identity when `ir.jit_info` is
  not the sticky `1`, so a deferred callee is re-probed (and inlines once it
  compiles) while a genuine over-cap or emitter refusal still caches a zero and
  is not re-probed per visit. Measured at that site (`direct_leaf`): `CallSlow`
  2,000,000 → **8** with `LeafCallProbe` 7 → 15; corpus `mean-jitGap` 68.4 →
  **65.2** over 77 rows/0 mismatches. **This stage also withdraws a Stage 1
  over-claim**: "100% of calls take `CallSlow`" was the aggregate counter
  dominated by a call site's *first* invocation, which `bench_once`'s two
  warmups exclude from the timing — later invocations already inlined. The
  defect is therefore narrower (a site's first invocation) and the fix's value
  is priced by the two lanes in one build: **`CallSlow` ≈ 48 ns/call against
  ≈ 6.4 ns inlined**, so it is a ~7.5× win for a single-entry long-running loop
  and invisible to `--jit-bench`/the corpus by construction. Two Cut 68 tests
  pinned the old policy and are amended to the new bounds (their measured probe
  counts went 1 → 9 and 2 → 10, both far from the ~100K a per-iteration
  re-probe would give); a new
  `installed_jit_a_deferred_leaf_call_site_stops_using_call_slow` pins the fix
  itself by counting `call_slow` at the site (≤ 40 of 100K, which a reverted
  fix fails). Gates: fmt clean, clippy workspace `-D warnings` clean, workspace
  **5,533 passed / 0 failed** (jit 211), runtime `--no-default-features` 952/0,
  v8 `simdutf` 380/0, `cli --no-default-features --features jit` green, test262
  `all` 48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s,
  `--jit-bench` unregressed. The eligibility half stays open and is now priced:
  G12 (`this`-slot leaves, every method call) and G13 (`leaf_lookup-miss`).
- **Stage 3, G12 the `this`-slot leaf (2026-09-28).** The probe refused
  `scope.this_slot.is_some()` outright, so every method call stayed on
  `call_slow`; it now takes the call's unbound receiver, applies
  `OrdinaryCallBindThis` (strict as-is; sloppy: an object as-is, nullish → the
  realm's global object, a primitive boxes and so still refuses), and fills the
  frame's `this` slot. The receiver went into `leaf_call_probe`'s signature, so
  the four-file mirror was paid (`JitSlowPaths`/`JIT_SLOW_PATHS`/the impl, the
  `Helper` entry and `JitHelpers` field plus the test double, `runtime_helpers`
  and `helpers_all`, and `emit_call`'s call site, which reuses `sig_call_slow`
  for the extra argument). Measured on `calls/method_call.js`: `CallSlow`
  14,000,000 → **8**, the row 132.7 → **77.9 ms (−41%)**, with the corpus's
  `mean-jitGap` reading inside the ±8 band its rounds show (65.2 → 73.6, and the
  same rounds' `mean-jlGap` moved too, so that is load rather than the change —
  the rows this cannot touch, like `opcost/js_call`, are unmoved). Correctness is
  a differential over method receivers, sloppy plain calls (→ global), strict
  plain calls (→ undefined), `call(null)`/`call(5)`/`call("x")` and a
  `this`-capturing closure: byte-identical between the modes. The new
  `installed_jit_a_this_slot_leaf_call_inlines` test compares against the
  interpreter and asserts `call_slow` stays small at the site; re-adding the
  refusal makes it count 200,000 and fail. Two things the build exposed and
  recorded rather than guessed: a body that reads `this` *and* an identifier is
  not leaf-certified at all (`LoadIdent` is leaf-excluded — G13), and the
  non-aliased frame re-probes on **every** call (`LeafCallProbe` 14,000,000 on
  this row), which is what caps the win at ≈39 ns/iteration instead of the
  ≈6.4 ns an aliased frame reaches — recorded as **G14**. Gates: fmt clean,
  clippy workspace `-D warnings` clean, workspace **5,534 passed / 0 failed**
  (jit 212), runtime `--no-default-features` 952/0, v8 `simdutf` 380/0, `cli
  --no-default-features --features jit` green, test262 `all` 48,464/0/0/0 of
  48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s, corpus parity 77 rows/0
  mismatches, `--jit-bench` in band.
- **Stage 3, G14 the non-aliased frame (2026-09-28).** A cache hit inlines a
  leaf in-frame only when the frame IS the argument region (`frame_size ==
  arity` with all args present); any other frame (a `this` slot, or a
  `var`/lexical slot past the params) fell back to the full probe, which
  re-ran `can_inline_leaf`/the realm check, `leaf_lookup`'s kind gate, the
  `lookup_info` consult and the 48-byte cache-record rewrite before rebuilding
  the frame — on every call. The hit path now calls a new `leaf_call_fill`
  (the probe minus that validation: it re-derives the scope, fills through the
  shared `bind_leaf_frame` the probe also uses, and returns the cached entry
  or 0 when the frame no longer fits, which routes to `call_slow`). The fill
  moved into one `bind_leaf_frame` helper so the probe's first visit and every
  hit cannot drift. `leaf_call_fill` is registered in all four mirror files
  (`JitSlowPaths`/`JIT_SLOW_PATHS`/the impl, the `Helper` entry and
  `JitHelpers` field plus its test double, `runtime_helpers`/`helpers_all`, the
  `emit_call` site — reusing `sig_call_slow`) and is added to
  `disturbs_leaf_eligibility`'s EXCLUDED list (a read plus a frame write, no
  re-entry, no compile). Measured on the plan's shape (`function f(x){var t=1;
  return x}` at 2M calls): `LeafCallProbe` 14,000,000 → **30**, the row 35.4 →
  **28.5 ms**, against an aliased control of **10.9 ms**; a frame-7 body shows
  the ~8.6 ns fixed residual plus ~0.7 ns/slot. Correctness: the new
  `installed_jit_a_non_aliased_leaf_call_fills_without_re_probing` compares
  against the interpreter and asserts `leaf_call_fill` does the per-call work
  (≥90,000 fills of 100,000) with the probe confined to the warm-up (≤200);
  routing the hit path back to the probe makes it count 0 fills and fail. This
  build also caught a self-inflicted hazard worth recording: the hit path's
  cache loads need `leaf_inline_offset`, and dropping it on one field read
  `callee_payload`'s high half as `arity` — a silent wrong answer that the
  fuzz-free unit tests surfaced only as failing values. Gates: fmt clean,
  clippy workspace `-D warnings` clean, workspace **5,535 passed / 0 failed**
  (jit 213), runtime `--no-default-features` 952/0, v8 `simdutf` 380/0, `cli
  --no-default-features --features jit` green, test262 `all` 48,464/0/0/0 of
  48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s, corpus parity 77 rows/0
  mismatches, `--jit-bench` in band (arithmetic 0.09, bare loop 0.11, function
  calls 0.13, non-leaf call 0.61).
- **Stage 3, G14's residual — the cached fill descriptor (2026-09-28).** G14
  had removed the per-call *probe*, but a floor build (whose `leaf_call_fill`
  returned the cached entry right after the cache check) showed the helper's
  body was still ~7 ns: the `leaf_lookup` into the agent's leaf cache, the
  `Rc<CompiledBody>` clone that ended the borrow, and the scope dereference.
  The probe now records the fill descriptor in the call record
  (`LeafInlineInfo.this_slot`/`strict`/`tdz_mask`/`fill_ok`) and
  `leaf_call_fill` rebuilds from it alone — no callee argument, no
  `leaf_lookup`, no clone — so the fill helper shrank from `sig_call_slow` to
  `sig_call`. Both the probe and the hit path fill through one
  `fill_leaf_frame(&LeafInlineInfo, &TdzSource, …)`: the probe passes
  `TdzSource::Store(&scope.tdz_store)` (exact for any frame), the hit path
  `TdzSource::Mask(tdz_mask)` (exact for `frame_size <= 64`; a wider frame
  records `fill_ok = 0` and the emitter keeps the probe fallback). The
  `OrdinaryCallBindThis` global now comes from a new `JitCallContext::
  global_bits`, snapshotted per call next to `global_object`, so the fill
  needs no `Agent` borrow at all — and the probe and the hit path bind the
  identical global. Measured: the plan's shape
  (`function f(x){var t=1;return x+t}` at 2M calls) 27.8 → **21.4 ms** —
  35.4 ms before G14 first landed — against an aliased control of 10.3 ms;
  `f2` (`return x`) 25.4 → 19.4 ms; `calls/method_call` 77.9 → **58.7 ms**,
  its first move since G12 because that row's `this` slot makes it
  non-aliased. Correctness: a differential over six frame shapes (a `var`
  slot, a `let` slot, a missing argument, a sloppy `this`→global, a strict
  `this`→undefined, a method reading its receiver) is byte-identical between
  the modes, and the existing
  `installed_jit_inline_leaf_call_with_var_slot_builds_the_frame` still passes.
  New `leaf_fill_descriptor_matches_the_scope` pins the mask/sentinel/`fill_ok`
  values; emptying the mask makes it fail. Gates: fmt clean, clippy workspace
  `-D warnings` clean, workspace **5,536 passed / 0 failed** (jit 213, runtime
  +1), runtime `--no-default-features` 953/0, v8 `simdutf` 380/0, `cli
  --no-default-features --features jit` green, test262 `all` 48,464/0/0/0 of
  48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s, corpus parity 77 rows/0
  mismatches, `--jit-bench` in band (arithmetic 0.09, bare loop 0.11, function
  calls 0.13, non-leaf call 0.64). The residual is now the frame build itself
  (~4 ns/call fixed + ~0.7 ns/slot) — the next lever would be a machine-code
  fill for the no-`this`/no-lexical case, which under this design would buy
  about a nanosecond over the inlined helper.
- **Stage 4, G13 the environment-using leaf (2026-09-28).** The gap the plan
  called "`leaf_lookup`-miss" turned out, on measurement, to be four different
  things — and only one of them the leaf lane. The audit (2M-call harness,
  release) is the §3 G13 table: `nested_read` is already the compiled
  `LoadIdent` cell probe at 2.6 ms, `recursive_fib` (310 ms) is a re-entrant
  callee a leaf can never be, `generator_loop` (90 ms) is resume machinery, and
  `closure_capture` (82 ms) is an env leaf whose site is *polymorphic* — which
  is now its own gap, **G15** (`LeafCallProbe` 21,900,000 for 1,000,000 calls).
  The slice that landed is the env leaf: a body reading a captured binding
  lowers the read to `Step::LoadContextSlot`, which `steps_are_leaf` does not
  exclude and the interpreter's own leaf gate has no condition against — so it
  was a certified leaf the compiled code could never call. The machine code
  cannot call it in-frame (the `body_context`/`lexical_env` swap must span the
  call and be undone before the caller resumes), so the probe now records
  `uses_env` (and still refuses, leaving the first call on `call_slow`) and the
  hit path gained an env lane: `leaf_call_fill` for the frame and room check,
  then a new `leaf_call_env` helper that re-derives the callee's environment,
  swaps it in, calls the compiled entry on the CALLER's ctx and buffer, and
  restores — mirroring `run_jit_leaf`'s env handling and tail in the same order.
  A frame wider than the record's TDZ mask records `entry = 0` so the site
  stays on `call_slow` rather than filling from a truncated mask. Measured on a
  monomorphic env leaf (`var base = 1; function inner(x){ return base + x; }` at
  2M calls): **92 → 40.5 ms (−56%)**, `CallSlow` 2,000,000 → **15**, against
  88 ms interpreted. Correctness: the corpus row amounts match `--jitless` on
  all four audit rows, and the new
  `installed_jit_an_env_leaf_call_runs_through_the_env_lane` counts `call_slow`
  at the site (refusing the env leaf again makes it count 100,000 and fail).
  `LeafCallEnv` is registered in the four mirror files and is NOT in
  `disturbs_leaf_eligibility` (the leaf's own compiled helpers bump the epoch
  from inside, on the caller's ctx). Gates: fmt clean, clippy workspace
  `-D warnings` clean, workspace **5,537 passed / 0 failed** (jit 214, runtime
  +1), runtime `--no-default-features` 953/0, v8 `simdutf` 380/0, `cli
  --no-default-features --features jit` green, test262 `all` 48,464/0/0/0 of
  48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s, corpus parity 77 rows/0
  mismatches, `--jit-bench` in band. Open from the audit: **G15** (the
  megamorphic site) — the row that made `closure_capture` look like a leaf
  gap.
- **Stage 4, G15 — the per-callee leaf-call record (2026-09-28).** G13's audit
  left `calls/closure_capture` unexplained, and the measurement was why: the
  call record holds ONE callee identity and its slot was a pure function of the
  site, so a site that sees N callees re-probed on nearly every visit
  (`LeafCallProbe`/`CallSlow` 7,000,000 for 1,000,000 iterations — the env lane
  never ran). A first slice folded the callee into the slot, which helped a
  2-callee site (70 → 27 ms) but not 64 — four slots cannot serve 64 callees.
  This lands the end state: the record is keyed by the CALLEE on the `Agent`
  (not inline in the per-run context, whose memset `recursive_fib` proved fatal
  at 329 → 732 ms), with a golden-ratio high-bit hash mirrored by the emitter
  and pinned by a test, and a code GENERATION so a record never outlives the
  compiled entry it names (a run bumps the agent's generation before its first
  `lookup_info`, which may evict; nested runs share it; a sweeping GC clears the
  table). `LEAF_CACHE` went 16 → 256 for the env lane's per-call env lookup.
  Measured: `calls/closure_capture` 88.8 → **50.9 ms** (`LeafCallProbe`/
  `CallSlow` 7,000,000 → ~900, `leaf_call_fill`/`leaf_call_env` 7,000,000),
  `construct_churn` ~120 → ~103, `recursive_fib` 329 → 278, no call row
  regressed. Gates: fmt clean, clippy workspace `-D warnings` clean, workspace
  **5,538 passed / 0 failed** (jit 215), runtime `--no-default-features` 953/0,
  v8 `simdutf` 380/0, `cli --no-default-features --features jit` green, test262
  `all` 48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s,
  corpus parity 77 rows/0 mismatches, `--jit-bench` in band (arithmetic 0.08,
  function calls 0.16, non-leaf call 0.50).
- **Stage 5, G1 — the inline dense-array element read (2026-09-28).** Stage 5's
  ordering was taken from the Stage 1 helper census, but a per-row instrument
  run first re-ranked it and falsified two of its items: G10's shape-free inline
  chain probe had already been measured and reverted (5-10% slower than the
  helper), and G11's `switch` is already 3.1x faster under the JIT than jitless.
  G1 survived: the read `a[i]` called `call_slow(GetMemberComputed)` for every
  shape while the write side had inline paths, at 73.4M helper calls over 12
  rows. `emit_dense_element_read` now inlines the dense-Array case (a tagged
  Object with a live `array_dense` cursor, a canonical index key below
  `elem_len`, a non-hole element — the same dense invariants the compiled append
  relies on); a hole (spec-absent → the chain), a spill, a typed array, a
  non-index/out-of-range key, and a non-Array decline to the helper. Measured:
  `arrays/index_loop` 37.9 → **20.7 ms** (`GetMemberComputed` 21,000,000 → 0),
  `objects/many_objects_read` 30.6 → **21.1 ms**, and their JIT-vs-interpreter
  ratios 3.3x → 6.0x and 3.5x → 5.1x. Two new tests: a helper-count proof
  (~100,000 → ~3 calls, mutation-checked) and a hole→chain exactness test.
  Gates: fmt clean, clippy workspace `-D warnings` clean, workspace **5,540
  passed / 0 failed**, runtime `--no-default-features` 953/0, v8 `simdutf`
  380/0, `cli --no-default-features --features jit` green, test262 `all`
  48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s, corpus
  parity 77 rows/0 mismatches, `--jit-bench` in band (arithmetic 0.07, function
  calls 0.17, non-leaf call 0.54). The remaining Stage 5 items are recorded in
  §5 as closed by measurement.
