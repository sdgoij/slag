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
| `SwitchTest` | 91.9M | 1 | one per `case`, per execution | **G19 — landed (Stage 7)** |
| `GetMemberName` | 90.4M | 54 | prototype-chain reads (every method read) | **new (G10)** |
| `GetMemberComputed` | 73.4M | 12 | computed read `a[i]`; the write side has two inline paths | G1 |
| `SetMemberSlot` | 70.0M | 3 | member writes off the in-place path (`this.x += n`, churn) | G1-adjacent |
| `LeafCallProbe` | 33.7M | 50 | the probe re-run on epoch churn — plus the dead-path tax of G2 | G2 |
| `LoadContext` | 30.3M | 4 | closure/context slot loads | — |
| `ForOfNextBindLocal` | 21.0M | 1 | `for-of` step | **G17 — landed (Stage 6)** |
| `SwitchDisc` | 21.0M | 1 | `switch` selector | **G19 — landed (Stage 7)** |
| `BreakControl` | 21.0M | 1 | `break` inside a `switch` | **G18 — landed (Stage 7)** |
| `ObjectFast` | 16.8M | 16 | object literal (fused — the sequence was already collapsed by Cut 72) | **G3 — landed (Stage 6)** |
| `TypedArrayLength` | 15.7M | 16 | array `length` read (misnamed: the FFI probe every `i < a.length` paid) | **G16 — landed (Stage 6)** |
| `ForInNext` | 6.3M | 1 | `for-in` step | G4 |
| `ConcatStrings` | 4.5M | 3 | string building: the `+` rope concat (direct helper) and the template append | **G5 — landed (Stage 6)** |

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
  21.0M). The compiled read now inlines both buffer-backed element shapes
  (`emit_element_read`, shared by the step `GetMemberComputed` and the register
  `GetMemberComputed`/`GetMemberComputedLocal`), dispatching on the receiver:

  - **A dense Array** (the `array_dense` cursor is live — non-null iff dense):
    a canonical index Number (`< 2^32-1`) below `elem_len` reads one buffer
    slot. A hole declines to the helper (spec-absent, so the chain must be
    consulted; a stored `undefined` is a real value and IS served), as do a
    spilled array, a string/out-of-range key, and a non-Array. Measured:
    `arrays/index_loop` 37.9 → **20.7 ms** with `GetMemberComputed` 21,000,000 →
    **0**, `objects/many_objects_read` 30.6 → **21.1 ms**, and their
    JIT-vs-interpreter ratios 3.3x → **6.0x** / 3.5x → **5.1x**.
  - **A numeric TypedArray** (`emit_typed_array_read_into`, the read mirror of
    `emit_typed_array_store_inline`, which is Uint8-only): `detached == 0`,
    `resizable == 0` (a fixed view's `array_length` is the effective length only
    while a resizable buffer has not shrunk), a non-null data pointer, a
    canonical index below `array_length`, then a load converted per element
    kind (every integer width plus Float32/Float64). Float16 (a soft-float
    decode) and BigInt64/BigUint64 (a BigInt allocation) decline. `immutable`
    is not checked — reads of an immutable buffer are legal. Measured:
    `arrays/typed_array` 50.1 → **35.6 ms** (`GetMemberComputed` 14,000,000 →
    **0**; 3.8x → **5.3x** vs the interpreter), `opcost/typed_array_for_each`
    78.2 → **68.4 ms**, and a dedicated `--jit-bench` row `typed-array read` at
    ratio **0.15** (89.5 → 13.0 ms). The lane is single-agent only: under the
    `workers` feature the shared-buffer block layout is cfg'd, so it declines to
    the helper exactly as the inline store does — which is why the
    workers-enabled `--workspace` test build and the test262 sweeps do not
    exercise it (the corpus/`--jit-bench` CLI build and `cargo test -p jit` do).
- **G16 — every array `length` read paid the typed-array FFI probe (landed,
  Stage 6).** The census counted `TypedArrayLength` at 15.7M over 16 rows and
  labelled it "typed-array `length`", which hid the shape: the helper is the
  FFI probe reached from `emit_member_cell_probe`'s `length` branch, and the
  compiled lowering sent the machine typed-array read's **miss straight to the
  FFI helper** before trying the dense-Array probe. A machine read hits only a
  fixed/live/writable IntegerIndexed receiver, so a plain Array — whose `length`
  is the slots cell — failed the machine gate, called `typed_array_length`
  (which answered its not-a-typed-array sentinel), and only then hit the dense
  probe. Every `i < a.length` loop bound therefore paid a helper round trip per
  iteration (measured at 14M calls and 14.8 → 13.6 ms on a plain
  `for (i = 0; i < 2e6; i++) s += a.length` shape, −8.4%). The fix is a
  lowering reorder, no new helper and no ABI change: the machine read's miss
  lands on the dense-Array probe, the dense probe's miss (a non-Array object)
  lands on the FFI probe, and a **non-Object** receiver skips the FFI probe
  entirely (the probe answers its sentinel for every non-Object unconditionally,
  so a primitive `length` — a String — no longer calls it either). Measured over
  the 77-row corpus: `opcost` 10.9M → **0.7M**, overall `TypedArrayLength`
  17.47M → **0.7M**; `--jit-bench` in band (`arithmetic` 0.09, `function calls`
  0.16, `non-leaf call` 0.50, `typed-array length` 0.18). New test
  `installed_jit_array_length_read_skips_the_ffi_probe` counts the helper through
  a wrapper and asserts the dense loop never calls it; detouring the dense hit
  through `ffi` makes it count 100,000 and fail (mutation-checked). Gates: fmt
  clean, clippy workspace `-D warnings` clean, workspace **5,543 passed / 0
  failed** (jit 220), runtime `--no-default-features` 953/0, v8 `simdutf` 380/0,
  `cli --no-default-features --features jit` green, test262 `all`
  48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s, corpus
  parity 77 rows/0 mismatches. No wasm sweep owed (`wasmtest` does not link
  `crates/jit`). A census label that names the helper, not the shape, is the
  trap here: "typed-array length" was really *array-length read*.
- **G17 — the compiled for-of fast-array cursor (landed, Stage 6).** The
  census's `ForOfNextBindLocal` (21.0M, one row) was the whole for-of advance:
  even the dense-array fast verdict (`ForOfState::FastArray`) paid a
  `call_slow` per element for `for_of_advance`'s two cache probes. `arrays/for_of_dense`
  ran 38.8 ms against an equivalent inline index loop's 22.1 ms — the for-of
  step was ~76% of the row. The fix is a **compiled cursor in two hidden frame
  slots** (the `alloc_hoist_slot` mechanism), carried on `Step::ForOfBegin`
  (`cursor: (array_slot, index_slot)`, `NO_FOR_OF_CURSOR` for a head without
  the fused bind): the begin helper seeds the array slot with the dense-Array
  Value (or `undefined`), and the compiled `ForOfNextBindLocal` reads
  `elem_ptr[index]` straight from the live `array_dense` cursor — the same
  invariants `emit_dense_element_read_into` relies on (a live cursor, the
  index below `elem_len`, a non-hole element) — writes the loop slot, bumps
  the cursor and takes the back edge with **no helper call and no value-stack
  round trip**. The Vm's `for_of_stack` entry stays authoritative for the
  shared core: the first decline (a hole, the end, a receiver that stopped
  being dense) abandons the cursor (the array slot is set to `undefined`, so
  every later step takes the helper) and runs the new `for_of_fast_next`,
  which syncs the entry's index to the one the inline path reached before
  advancing it through `for_of_advance`. A frame-slot cursor is what makes the
  protocol safe: it survives nesting (each loop owns its slots), recursion
  (each frame owns its slots) and suspension (the frame is saved), unlike a
  per-context cursor.

  Measured: `arrays/for_of_dense` **38.8 → 8.0 ms** (result byte-identical),
  the row's `ForOfNextBindLocal` **21.0M → 0** with `ForOfFastNext` at one call
  per loop entry (the Done decline), and the `arrays` family's `mean-jitGap`
  28.72 → 21.54. Two new tests:
  `installed_jit_dense_for_of_inlines_the_cursor` counts the per-element helper
  through a wrapper and asserts it never runs (forcing the helper path counts
  110,000 and fails, mutation-checked), and
  `installed_jit_dense_for_of_matches_the_interpreter` folds a dense array, a
  middle hole, a trailing hole (`length` past `elem_len`), a sparse array, a
  Set / String / TypedArray, a mid-loop grow and shrink, a `break`, nesting,
  and a captured per-iteration `const` into one checksum compared against the
  interpreter. Gates: fmt clean, clippy workspace `-D warnings` clean,
  workspace **5,545 passed / 0 failed** (jit 222), runtime
  `--no-default-features` 953/0, v8 `simdutf` 380/0, `cli
  --no-default-features --features jit` green, test262 `all`
  48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s, corpus
  parity 77 rows/0 mismatches, `--jit-bench` in band. An `--gc-stress` run of a
  for-of workload (including a middle hole and a `push`-per-iteration) is
  byte-identical between jit and jitless, and a generator `yield`-ing inside a
  for-of exercises the frame cursor across a suspension. No wasm sweep owed
  (`wasmtest` does not link `crates/jit`).
- **G3 — the object literal's shape walk (landed, Stage 6; the rest of the
  gap is the allocator).** The plan's G3 premise ("a helper sequence per
  element/step/property") was already addressed by Cut 72: `Step::ObjectFast`
  lowers to ONE helper call that resolves the literal's shape and adopts every
  field. Measuring the residual split the cost by shape (3M allocations per
  row, release, repeatable): `{}` **53 ns**, `{a,b,c}` **84 ns**, `{a…h}`
  **114 ns** — i.e. **54 ns is the fresh `JsObject` itself** (528 B, every
  field written by `init_ordinary`, ~1.6 GB moved by the probe) and the rest is
  ~9 ns per key. So the literal is ~64% fresh-object write bandwidth at the 3
  key shape, and **no inline path can move that**: it needs a smaller object or
  allocation sinking, both `crux` concerns.

  The removable piece is the per-key `Map::get_or_create_child` walk (a
  hash + probe per key). `Map` now carries a **composite transition cache**
  (`composite: HashMap<Box<[AtomId]>, Handle<Map>>`, bounded at 1024 entries,
  traced like `transitions`): `object_fast_create` resolves the whole vector-free
  head with one lookup and records the shape on a miss, so a literal pays one
  hash + probe instead of N — never more (a 1-key literal is a wash, a 16-key
  one saves ~15 lookups), and the entries keep their shapes alive through
  `back_pointer`. Measured: `{a,b,c}` **84 → 74 ns (−12%)**, `{a…h}` **114 → 100
  ns (−12%)**, `{}` unchanged (53 ns), `objects/destructure` 176 → 166 ms (−5%).
  Over the 77-row corpus this is inside the run-to-run noise (mean-jitGap 71.2
  vs 68.4 across two runs, both 0 mismatches), which is the honest caveat: the
  primitive improves, but literal-heavy corpus rows are allocation-bound and
  only `destructure`/`spread_assign` exercise it. New crux test
  `composite_child_caches_the_whole_literal_walk` pins that the cached shape is
  exactly the walk's, is keyed by the full list, and is not populated by the
  walk itself. Gates: fmt clean, clippy workspace `-D warnings` clean,
  workspace **5,546 passed / 0 failed**, runtime `--no-default-features` 953/0,
  v8 `simdutf` 380/0, `cli --no-default-features --features jit` green, test262
  `all` 48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s,
  corpus parity 77 rows/0 mismatches, `--jit-bench` in band. **The next lever
  here is not a JIT arm**: it is `JsObject`'s size (528 B, 128 B of it
  `in_fields`) or allocation sinking, and it is recorded as such so G3 is not
  re-opened as an inline path.
- **G5 — string building: mostly landed before this arc, plus the template
  append (landed, Stage 6).** The big G5 win was already in the tree (2026-09-07,
  `.notes/perf.md`): `crux::number::to_string` writes an exactly-representable
  integer's decimal directly (no ryu + digit-vector rebuild) and `Add` with one
  string operand and a Number/Boolean/Null/Undefined fuses to a two-string
  concat in `apply_binary` and the register executor — `coercion_concat` went
  ~320 → ~160 ms then (103 ms jit today), and `ConcatStrings` (string+string)
  is already a direct helper with no `apply_binary` round trip, so it is a
  rope allocation and nothing else. What remained was the **template append**:
  both the interpreter handler and the JIT helper rebuilt the accumulator flat
  per substitution (`string_units_of` → `Vec<u16>` → `from_utf16`), so a k-part
  template was O(k·len) — quadratic for a build loop. Both now go through one
  shared `concat_template`/`concat_template_const` that appends with
  `JsString::concat`, a rope concat: O(k), and still flat at or under the flat
  threshold, so a short template keeps the leaf representation it had. Measured:
  an 18-substitution build loop **929 → 473 ms (−49%)**, a 4-substitution one
  **131 → 90 ms (−32%)**, and the real fixture
  `language/expressions/template-literal/evaluation-order.js` **371 → 336 ms
  (−9.5%)**. Content is identical (UTF-16 throughout, so lone surrogates
  survive: a jit/jitless/node three-way on `\uD83D`, an astral pair and
  `String.prototype.slice` agrees exactly). The `strings` family's remaining
  gap to node is the builtin-call path (a method read + `CallSlow` per
  `s.charCodeAt`), not string building — that is G10/CallSlow territory.
- **G3/G4/G5 status.** G3 landed (above, with its residual in `crux`), G4 landed
  as **G17**, and G5 landed as the template append above.
- **G4's residue — the non-fused `ForOfNext` / `ForInNext` — closed by
  measurement, not landed.**

  - **`ForOfNext` (the stack variant) has 0 corpus calls.** It is emitted only
    when the for-of head binding is a CONTEXT slot, which happens only when a
    closure in the body captures the head — and a captured head emits
    `EnterPerIteration`/`PerIteration`, i.e. a spec-required fresh environment
    plus a closure allocation per iteration. The shape is therefore
    allocation-bound (the same reason the for-in head-let row is), so inlining
    the advance cannot pay; the G17 cursor is allocated for every certified
    for-of anyway, so the change would be small, but with no measurable shape
    behind it the plan's bar is not met.
  - **`ForInNext` is 9.1M over one row, but the compiled step is within ~3.5%
    of the interpreter.** The for-in `fast` verdict and the generation-skip
    per-key check already landed (2026-09-06, `.notes/perf.md`), as did the
    `ForInBegin` enumeration cache, so `control/for_in.js` is 34.9 ms jit /
    49.7 ms jitless and the protocol is *not* the row's cost. Isolating it
    (150k re-entries of a 5-key loop): a for-in with no computed read is
    **8.53 ms jit** against an equivalent plain `for (i = 0; i < 5; i++) n += 1`
    loop at **1.82 ms**, so the whole protocol is 6.7 ms — but the interpreter's
    is 5.5 ms (19.03 − 13.56), i.e. **the compiled step's only deficit is the
    helper call, ~1.2 ms of a 34.9 ms row (~3.5%)**. Inlining it needs a
    FIVE-slot frame cursor (keys pointer/length/index, base pointer,
    generation) plus a machine-readable `keys` element layout, and the row's
    real cost is the `o[k]` computed read on a non-Array object
    (`GetMemberComputed` 5.25M in the same directory — a per-site-IC surface,
    Stage 7), not the protocol. Recorded so it is not re-opened as an inline.
- **G8 — the computed read's per-read helper (landed, Stage 7).** The
  census's `GetMemberComputed` (73.4M, 12 rows) was the whole helper, not the
  interning: the same read *named* is 7.0 ns with zero helper calls (machine
  code), *computed* was 35.1 ns with a helper per read, and neither key length
  (30-char keys) nor key kind moved it. The landed fix is a **key → atom cell**
  the compiled probe consults BEFORE the key is converted: `computed_read_cells`
  maps a String box's identity to its interned atom (both immutable, so the map
  never needs invalidation), and the probe then reads the VALUE from
  `member_value_cells` — the cell every store path already maintains — so a hit
  is exactly the interpreter's own `member_cell_get`. The first cut cached the
  value in the key cell and was falsified by the corpus (an in-place value write
  does not bump the generation), see §8. `str_key.js` 52.7 → 10.6 ms and the
  real-world `control/for_in.js` **34.9 → 10.54 ms** — the row's `o[k]` cost,
  now inline. Number-keyed `o[i]` is not served (the cell is String-keyed by
  design), the same limitation the interpreter has.
- **G18 — the compiled `break`/`continue` transfer (landed, Stage 7).** Every
  compiled `Step::Break`/`Continue` ran the `control_transfer` helper *and* an
  epoch bump, even when the transfer is exactly `ip = target` (no finally to
  route through, no for-of iterator to close). With the Lowerer's
  `has_try`/`has_for_of` and a new `has_async_for_of` all false the arm now
  jumps directly — `control/switch_dispatch.js` 63.5 → 42.2 ms jit and
  `BreakControl` 21.0M → 0 (§8). **G19 landed the rest of the row** (§8): the
  discriminant is a machine-readable `Value` + presence flag and the case test an
  inline strict-equality, so `control/switch_dispatch.js` is 23.7 ms jit (8.9×
  jitless) with `SwitchDisc`, `SwitchTest` and `BreakControl` all at 0.
- **G19 — the `switch` discriminant and case test (landed, Stage 7).** G18 left
  the row's `SwitchDisc` 21.0M + `SwitchTest` 91.9M, both helper calls for what
  is a store and a compare. `Vm::switch_disc: Option<Value>` was the blocker (not
  machine-readable), so the field became a raw `Value` plus `switch_disc_set`;
  the compiled `SwitchDisc` now writes it with two stores and the compiled
  `SwitchTest` compares inline with `===`'s three fast paths, falling to the
  helper for distinct string/BigInt boxes and mixed kinds (§8).
- **G20 — the dense-array element overwrite (landed, Stage 7).** The compiled
  computed store inlined only the APPEND shape (`index == length`), so
  `a[i] = v` over an already-filled array took the general helper on every
  store (`SetMemberComputed` 6.2M — 77% of it `opcost/element_write.js` — plus
  `FastArrayElementWrite` 2.25M). The new `emit_dense_element_store_into` arm
  (wired into both the step and the register store tails) mirrors the read
  arm's gates — canonical index, `idx < elem_len`, non-hole — plus the append's
  write discipline: no attribute or chain check while dense, a registered
  prototype declines (the generation bump advances the elements protector), and
  the write stays inline only when no barrier can be needed (a young container
  or a non-heap value). It writes `elem_ptr[idx] = value` and bumps the
  generation with no length/cursor change — exactly the interpreter's
  `array_element_write_dense` in-place branch. `opcost/element_write.js`
  1.84 → 1.17 ms, and both store helpers left the census top (§8).
- **G21 — the compiled non-leaf call — landed (Phase 1: the runtime self-call
  path).** A compiled `CallSlow` site whose callee IS the running closure now
  runs the callee's compiled body directly, with a nested frame in a private
  buffer, instead of re-entering the interpreter's own call machinery
  (`do_call_fast` -> `ordinary_call` -> `run_compiled_body` -> `run_jit_body`:
  a fresh Vm take/return, an execution-context push, a frame setup, a
  global-shadow walk, a root registration and a whole new ctx) per recursion
  level. The mechanism lives in the runtime (`call_slow`'s `self_call_inline`,
  `crates/runtime/src/jit.rs`), not in the emitter: the existing helper funnel
  already receives the shared ctx and the argument region, so the slice is
  runtime-only (no new helper, no four-file mirror, no Cranelift changes) and
  is gated by `CompiledBody::self_call_eligible` (`crates/runtime/src/ir.rs`):
  a certified body with no `this`/`arguments` slot, no per-call capture
  context, and no try/for-in/for-of/destructuring/suspension step — the nested
  activation then shares the caller's environment, working-area bound and
  array-index cursor unmodified. Measured: `recursive_fib.js` **~290 -> ~43 ms
  jit** (14.7x jitless), corpus parity 77/77. The `--jit-bench` `non-leaf call`
  ratio quoted for Phase 1 was a **misread and is retracted** (§8): its *jit*
  time was unchanged (~13.9 ms) and the ratio moved with the load-sensitive
  interpreter column.

  **Phase 2a landed (the emit-side self gate).** A call site in a scoped body
  now compares the resolved callee against `JitCallContext::current_function`
  in machine code and branches to the `call_slow` lane, skipping the ~15-20
  instruction leaf-record gate — the probe could only refuse such a callee (the
  running body contains this call step, so it is not a leaf). `recursive_fib.js`
  **~43 -> ~39.7 ms** (8%), leaf-call rows unregressed. **Phase 2b, the large
  remaining lever:** the *non-self* call. `--jit-bench`'s `non-leaf call`
  (`bench` -> `mid` -> `leaf`, no self-call) is **~140 ns per call at ~13.9 ms
  jit vs 6.2 ms jitless**, and neither Phase 1 nor 2a touches it: a general
  compiled call still pays the full `ordinary_call` -> `run_compiled_body` ->
  `run_jit_body` funnel (Vm pool take/return, execution-context push,
  `globals_unshadowed` chain walk, ctx build, 64-slot work-buffer init, root
  registration). **A correctness bug in Phase 1 was found and fixed while
  beginning 2b (2026-09-28, §8):** a self-eligible body whose base case is a
  *strict tail call* corrupted the caller, because the nested `Helper::TailCall`
  runs `tail_prepare_ordinary` on the **shared** Vm and sets `ctx.tail`, which
  the self path ignored. That is the hazard class 2b must not repeat: **2b must
  run the callee on a pooled (private) Vm, not the caller's**, and cache a
  per-callee descriptor — the compiled entry/scope, the closure **environment**
  (`Function::environment`, fixed per closure), and `globals_unshadowed` (also
  fixed per closure, since a certified body's env chain to the global is
  immutable) — so the hash lookup, globals walk, ctx build and Vm reset are
  skipped without sharing state. That is its own design pass, not a Phase-1
  widening.
- **G6/G7 and the feedback record — cell capacity, the register path's one shape,
  per-site feedback.** G8 landed (above, Stage 7) as a key→atom cell, not the
  value cell the census implied, because the in-place store discipline forbids a
  generation-validated value cache (§8). G6 (capacity for the colliding named
  cells; widening `MEMBER_CELLS` 16→256 was measured and reverted — it cost hot
  cache locality), G7 (the register path's one shape) and the per-site feedback
  record remain: G9, G10 and G11 are each a *missing inline path*, and doing
  them one at a time is how the current asymmetry (an inline path for `+` but
  not `<<`, for an own read but not an inherited one) came to exist.

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
  **G16 (the array `length` read) and G17 (the for-of fast-array cursor, §3)
  landed first**, because a per-row instrument run on the current tree
  re-ranked the census: the `length` read was the least discussed but the
  cleanest to fix (a lowering reorder, no new helper), and the for-of step had
  the largest measured headroom (`arrays/for_of_dense` 38.8 ms against an
  equivalent inline index loop's 22.1 ms). One Stage-6 item was re-measured
  and is **closed by measurement**: the `for-in` head-let row
  (`language/statements/for-in/head-let-fresh-binding-per-iteration.js`, the
  slowest corpus row at ~950 ms/rep) is **allocation-bound**, not a JIT gap —
  `PerIteration` builds a spec-required fresh environment and a closure per
  iteration, and the row runs **952 ms JIT against 1,058 ms jitless** (~10%),
  so no inline path can pay. Still open: G5 (string
  building) and the non-fused `ForOfNext` / `ForInNext` advances. **G3 (object
  so no inline path can pay. **G3 (object literals) landed** as the composite
  literal-shape cache (§3 G3): the fused `ObjectFast` create was already one
  helper call (Cut 72), so the slice removed the per-key transition walk (−12%
  on a 3- and 8-key literal, −5% on `objects/destructure`), and the residual —
  the fresh 528 B `JsObject` write, 54 of the literal's 84 ns — is recorded as a
  `crux` concern (object size or allocation sinking), not a JIT arm. **G5
  (string building) landed** as the template-append rope (§3 G5): the
  coercion/integer path had already landed in 2026-09-07, and the remaining
  `string_units_of`/`from_utf16` rebuild per substitution was quadratic —
  18-substitution build loop 929 → 473 ms. **The non-fused `ForOfNext` /
  `ForInNext` are closed by measurement** (§3 G4's residue): `ForOfNext` has 0
  corpus calls (a captured head, which forces per-iteration envs and closures —
  allocation-bound), and the compiled `ForInNext` is within ~3.5% of the
  interpreter (its deficit is the helper call alone, 1.2 ms of the 34.9 ms
  `control/for_in.js` row, whose real cost is the `o[k]` computed read). That
  completes Stage 6.
- **Stage 7 — G6/G7, G18/G19, cells and feedback.** Capacity for the colliding
  cells, the register path's one shape, and a per-site feedback record so the
  inline paths above become a mechanism rather than a set of bespoke arms.
  **G8 landed first** (§3, §8): the computed read's per-read helper became a
  key → atom cell feeding the existing member value cell — `str_key.js`
  52.7 → 10.6 ms and the real-world `control/for_in.js` 34.9 → 10.54 ms. **G18
  landed next**: a compiled `break`/`continue` with no finally or for-of in
  scope jumps directly — `control/switch_dispatch.js` 63.5 → 42.2 ms.** G19
  (the switch discriminant + per-case strict-equality inline) then took that
  row's remaining helper share to zero — 23.7 ms jit, 8.9x jitless. **G20 landed
  next** (the dense-array overwrite: `opcost/element_write.js` 1.84 -> 1.17 ms),
  and **G21 (the compiled non-leaf call) landed** as a runtime self-call fast
  path inside the existing `call_slow` funnel (§3, §8): the row's `CallSlow`
  helper count is unchanged by design (the path lives inside the helper), but
  `recursive_fib.js` fell ~290 -> ~43 ms, and **Phase 2a (the emit-side self
  gate) followed immediately**: a self-call site now skips the leaf-record gate
  in machine code, ~43 -> ~39.7 ms (~8%). The `--jit-bench` `non-leaf call`
  ratio originally quoted here (0.52 -> 0.40) is **retracted** (§3, §8): that
  row is a *non-self* call and its jit time never moved; the ratio followed the
  load-sensitive interpreter column. That row — ~140 ns per call — is Phase 2b,
  the general call (§3).
- **Stage 8 — the general compiled call (G21 Phase 2b).** The last call-side
  deficit: a call to a *different* certified function still runs the full
  `ordinary_call` -> `run_compiled_body` -> `run_jit_body` funnel, and
  `--jit-bench`'s `non-leaf call` is ~140 ns/call. **Status: 8.0 measured; 8.1
  is not worth a slice and 8.2 is not to be built — the redesign that is, is
  Stage 9 below.** Phase 1's
  shared-Vm/shared-ctx shape is explicitly **not** the vehicle — its tail bug
  (§8) showed why: the
  callee's body, closure environment, `globals_unshadowed`, and tail/suspend
  handling all differ from the caller's, and a nested run on the caller's Vm
  corrupts it. The design is a **pooled private Vm** plus a cached per-callee
  descriptor, in three parts:
  - **8.0 — measured (2026-09-28); the result cancels 8.1/8.2 as designed.**
    Three probes, all temporary (removed; the tree is byte-identical above them):
    (a) isolated per-op costs from a release micro-bench — `take_vm` +
    `return_vm` (incl. `Vm::reset`) **12.8 ns**, the `ExecutionContext` push+pop
    **14.9 ns**, the `globals_unshadowed` walk **2.4 ns**, the `ecma_functions`
    hash on a hit **0.8 ns**, the `JitCallContext` build **3.0 ns**, a TLS
    push/pop **1.4 ns**; (b) region timers showing the whole `call_slow`
    interpreter path is essentially the entire per-call cost (~138 ns/call,
    matching the row) — the emit-side probe and the helper entry are minor; and
    (c) the remainder (~100 ns) is diffuse: the `do_call_fast`/`call_inner`/
    `ordinary_call` dispatch arms and `run_compiled_body`/`run_jit_body`'s setup
    (frame, root, ctx, work), with no single dominant item. **The two biggest
    single costs — the per-call Vm take/return+reset and the `ExecutionContext`
    push/pop — are structural to running each JS call on its own Vm and
    execution context, and a private-Vm lane (8.2) keeps both.** What 8.1/8.2
    can actually remove is the hash (0.8 ns), the walk (2.4 ns), the intrinsic
    read and some dispatch — ~10-15 ns of ~138, about **10%**, for a large and
    hazardous lane. So **8.1 is not worth a slice and 8.2 should not be built
    as designed**; the real lever is architectural (run JS frames on one
    Vm/stack, V8-style), which is out of this plan's scope. Recorded as a
    measured negative result, per §6's rule.
  - **8.1 — the per-callee descriptor (not built; 8.0 cancels it).** A new `Agent` table keyed by callee
    identity, the `LeafCallRecord` shape, holding what is *fixed per closure*:
    `entry`, `stack_usage`, `body: *const CompiledBody` (for `scope`),
    `globals_unshadowed` (a certified body's env chain to the global is
    immutable, so the verdict is stable), the apply/call intrinsic bits when
    `has_call_apply`, and — only under `realm_count == 1` — the global. The
    closure environment is the one field to be careful with: `ordinary_call`
    already holds it (`data.environment`), but a *call-site* lane has only the
    callee value. Reading it off the closure needs a new `Function::environment`
    getter and must handle the registration elision (`set_environment_edge` is
    called only when the env is NOT `realm.global_env`, so `None` means "the
    global env"); caching it in the table instead makes the table a GC edge
    source that must be traced and sweep-cleared. Choose one and record why —
    do not assume the field is always populated. Validation reuses
    `LeafCallRecord`'s exact-match identity plus `code_gen` (an evicted entry
    must not be reused) and a clear on any sweep. This part
    alone lets `run_jit_body` and `ordinary_call` skip the hash, the walk and
    the intrinsic read, and it is landable on its own (measure the funnel after
    it).
  - **8.2 — the private-Vm lane (not to be built; 8.0 shows it keeps the two
    biggest costs).** For a callee the descriptor marks runnable,
    run its entry on a **pooled Vm** (`take_vm`/`return_vm`), with a *fresh*
    `JitCallContext` built from the descriptor and never the caller's, a frame
    built like `run_leaf_body` (params/vars/TDZ, plus `this` when `this_slot`
    and `arguments` when `arguments_slot`), a capture context from the closure
    env when `context_names` is non-empty, and the body's own work buffer. The
    `ctx.tail` outcome loops on `vm.tail_replaced` (`run_compiled_body`'s loop);
    a suspension returns `DISPATCH_SUSPEND` (a plain certified function has
    none, and a generator/async reaches its driver through `call_inner` first,
    so those are gated). The emitter change is a gate on the callee's
    *descriptor* eligibility, like Phase 2a's; a miss falls to `call_slow`'s
    interpreter path unchanged.
  - **Acceptance.** `--jit-bench`'s `non-leaf call` *jit* milliseconds fall
    (never the ratio — §8), and nothing regresses. Differential tests over a
    non-self callee with captures, a `this` receiver, `arguments`, a tail call
    inside the callee, and a throwing callee (so the pooled-Vm path's
    `ExecutionContext`/`dispose_env_resources` fidelity is checked); a
    `--gc-stress` call-heavy shape; the descriptor-invalidation mutation-check.
  - **Traps.** The descriptor table must be cleared on any sweep (its key is a
    callee box address, recyclable — the same reason `leaf_records` is cleared);
    the pooled Vm must be an active run (`with_jit_run`) for the call's
    duration so a mid-call collection traces it; the general call **is**
    stack-observable (unlike the leaf path), so the `ExecutionContext` push and
    `dispose_env_resources` must be preserved or a throwing callee's `e.stack`
    and its `using` disposal regress; and the pooled Vm's `Vm::reset` cost is
    part of what 8.0 measures, because it may make the whole lane a wash.

- **Stage 9 — the one-Vm frame-record redesign (the call-side architecture).**
  This *is* the "real lever" the call arc names: make a JS frame a compact,
  stack-allocated record on **one** Vm, so a call is a push/pop rather than a
  Vm `take_vm`/`return_vm`+reset. It replaces the cancelled 8.1/8.2.
  - **Why sharing the Vm (8.2) does not pay — measured, not guessed.** The
    per-frame `Vm` state a shared lane must save/restore is ~15 fields even for
    a certified, no-try/for-of/suspend/vector/tail body (`ip`, `lexical_env`,
    `body_context`, `current_function`, `current_new_target`, `args`,
    `completion`+`completion_is_empty`, `switch_disc`+`switch_disc_set`,
    `chain_short`, `globals_unshadowed`, `strict`, `tail_replaced`,
    `pending_call`, `stack.len`, `jit_roots`), and the whole `Vm` is ~45. A
    15-field save/restore is 10-15 ns — the same as the pooled take/reset
    (13 ns) it removes — so 8.2 nets ~zero. **The fix is not to share the Vm;
    it is to make the frame a record** (below).
  - **Cut 1 — landed (2026-09-29): the certified-callee lane, and it is worth
    2.7× on the row.** `call_slow` gained a second fast path beside the self
    path (`self_call_inline`): a call to a *different* certified body resolves
    the callee's record, takes the same gate (`self_call_eligible` plus a new
    `private_frame_safe`), and runs its compiled entry directly on the caller's
    ctx with a nested frame in a private buffer and its own `JitCallContext` —
    no `do_call_fast`/`ordinary_call`/`run_compiled_body`/`run_jit_body`, no
    pooled-Vm take/reset, no `ExecutionContext` push. `Vm::save_scratch`/
    `restore_scratch` isolate the per-activation registers (`ip`, `acc`,
    `loop_counter`, `loop_num`, the string builder, `completion`+
    `completion_is_empty`, `switch_disc`+`switch_disc_set`, `chain_short`) and
    the lane saves/restores the five control fields (`lexical_env`,
    `body_context`, `current_function`, `current_new_target`, `strict`). The
    self path uses the same `save_scratch` now (it had the same latent
    clobber). Measured, A/B in one tree (`false &&` on the dispatch):
    `--jit-bench` `non-leaf call` **14.59 → 5.40 ms** (ratio 0.54 → 0.21).
    Three soundness rules fell out of the gates and are enforced by the gate
    and the tests, not by hope:
    - **A `vm.frame`-reading helper cannot run on a private-buffer frame.**
      `builder_bind`/`builder_store`/`create_function_decl` read
      `Vm::frame_get`, which the private buffer does not own, so a body with a
      planned `s += e` loop or a hoisted block function declaration is refused
      (`private_frame_safe`). A test caught this (a `mid`-style callee whose
      `s += n` loop silently built into the caller's frame slot and returned
      `0`), which is why the predicate exists.
    - **Compiled binding reads must resolve from `vm.lexical_env`, not the
      `ExecutionContext`.** `load_ident`/`typeof_ident`/`resolve_var_ident`/
      `update_ident` called `resolve_binding`, which reads
      `agent.running_context()` — the *caller's* env, since the lane pushes no
      context. `context::resolve_binding_from(env, …)` is the fix; it is also
      behavior-preserving for the funnel path (where the two envs are equal).
      Without it the whole `resizable-buffer` fixture family failed with
      `ReferenceError: "<global>" is not defined` under the lane.
    - The gate stays conservative: `this`/`arguments`/capture contexts, try,
      iterators, destructuring, suspension, tail and vector calls, generators,
      async functions, class constructors and non-ECMA callees all fall back to
      the funnel (which also ports and promotes them).
  - **Cut 2 — the `ExecutionContext`.** ~15 ns a call and load-bearing for the
    callee's env and for `e.stack`; the leaf path already omits it and still
    names frames, so cut 2 is to (a) set `vm.lexical_env` directly (not via
    `running_context`) and (b) name frames from the frame stack (a `function` +
    `source` on the record, or a lightweight push), then drop the per-call
    `ExecutionContext` for certified calls.
  - **Cut 3 — the interpreter's activations** use the same frame stack, and
    cut 4 removes the pool entirely.
  - **Cut 0 — done (2026-09-29): the cost is a chain, not one ~100 ns item,
    and it is safe to aim the redesign at the whole chain.** The ablation plan
    was replaced by an `rdtsc` exclusive-time profiler over 14 regions on the
    release `non-leaf call` row (timing-only; removed again, `git diff crates/`
    empty above it). Two facts first: `mid` **is** compiled — the callee's
    machine code runs on every call (`jit_code` entries == call count) — and
    there is no interpreter fallback (`vm.start` ran 16 times, the
    pre-promotion warm-up). The instrumented row is 294.6 ns/call against the
    uninstrumented 138 ns; the profiler's own ~11 ns per region-pair (14
    pairs, ~157 ns) is the overhead, so leaf regions are pure and an
    intermediate carries its children's entry/exit cost. Per-entry exclusive
    ns (share): `run_compiled_body` 42.4 (14.4%), `run_jit_body` 40.0 (13.6%),
    `ordinary_call` 28.1 (9.5%), `jit_code` 28.0 (9.5%), `complete_pending`
    27.6 (9.4%), `do_call_fast` 24.9 (8.4%), `call_slow` 23.7 (8.1%),
    `return_vm` 18.8 (6.4%), `with_jit_run` 18.5 (6.3%), then `globals_walk`
    8.9, `lookup_info` 8.8, `setup_frame` 8.6, `take_vm` 8.1, `new_body_ctx`
    8.0 (2.7-3.0% each). No region exceeds 14.4%, and the three clusters —
    the interpreter round-trip (`do_call_fast`+`complete_pending`+
    `ordinary_call`), the pooled-Vm round trip (`take_vm`+`return_vm`, both
    pure readings, a real ~27 ns rather than the 13 ns the earlier isolated
    probes suggested), and the JIT harness (`run_compiled_body`+
    `run_jit_body`+`lookup_info`+`globals_walk`+`new_body_ctx`+`setup_frame`+
    `with_jit_run`) — are all comparable. So the fix is to **collapse the
    chain**, which is what the certified-call lane does: it removes the whole
    interpreter round-trip and the pooled-Vm round trip at once rather than
    shaving one item. The earlier "sharing the Vm does not pay" argument was
    about a nested run **using `vm.frame`** (which must save 15 fields); the
    self-path shape uses a private buffer and needs none of `ip`/`args`/
    `completion`/`switch_*`/`tail_replaced`/`pending_call`.
  - **Traps:** the frame stack must be GC-traced (its values are live);
    `new_body_context` per frame; error-stack fidelity (cut 2); suspension and
    tail interaction (initially gated out); and `agent.jit_depth`/`ACTIVE_RUNS`
    must follow the frame stack, not a per-call Vm.

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
- **Stage 5, G1 — the inline computed element read (2026-09-28).** Stage 5's
  ordering was taken from the Stage 1 helper census, but a per-row instrument
  run first re-ranked it and falsified two of its items: G10's shape-free inline
  chain probe had already been measured and reverted (5-10% slower than the
  helper), and G11's `switch` is already 3.1x faster under the JIT than jitless.
  G1 survived: the read `a[i]` called `call_slow(GetMemberComputed)` for every
  shape while the write side had inline paths, at 73.4M helper calls over 12
  rows. `emit_element_read` now inlines both buffer-backed element shapes,
  dispatching on the receiver: a **dense Array** (a live `array_dense` cursor, a
  canonical index key below `elem_len`, a non-hole element — the same dense
  invariants the compiled append relies on) and a **numeric TypedArray** (the
  read mirror of the Uint8-only inline store: not detached, not resizable, a
  non-null data pointer, a canonical index below `array_length`, then a load
  converted per element kind — every integer width plus Float32/Float64, with
  Float16 and the BigInt kinds declining). A hole (spec-absent → the chain), a
  spill, a non-index/out-of-range key, and a non-Array decline to the helper.
  Measured: `arrays/index_loop` 37.9 → **20.7 ms** (`GetMemberComputed`
  21,000,000 → 0), `objects/many_objects_read` 30.6 → **21.1 ms**,
  `arrays/typed_array` 50.1 → **35.6 ms** (`GetMemberComputed` 14,000,000 → 0),
  `opcost/typed_array_for_each` 78.2 → **68.4 ms**; the rows' JIT-vs-interpreter
  ratios 3.3x → 6.0x, 3.5x → 5.1x, 3.8x → 5.3x; and a new `--jit-bench` row
  `typed-array read` at ratio **0.15** (89.5 → 13.0 ms). Three new tests: a
  dense helper-count proof (~100,000 → ~3 calls, mutation-checked), a dense
  hole→chain exactness test, and a typed-array pair (a helper-count proof,
  mutation-checked, plus a nine-element-kind exactness battery covering the
  declines). A trap found here: `crates/test262`/`crates/v8` enable
  `runtime/workers`, so a `--workspace` test build unifies the feature ON and
  the typed lane (correctly) declines — the inline assertion is therefore gated
  on `!crux::typed_array::WORKERS` and is only meaningful under `cargo test -p
  jit` and the CLI builds. Gates: fmt clean, clippy workspace `-D warnings`
  clean, workspace **5,542 passed / 0 failed** (`-p jit` 219/0, which is where
  the inline assertion bites), runtime `--no-default-features` 953/0, v8
  `simdutf` 380/0, `cli --no-default-features --features jit` green, test262
  `all` 48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s,
  corpus parity 77 rows/0 mismatches, `--jit-bench` in band (arithmetic 0.08,
  function calls 0.16, non-leaf call 0.50). The remaining Stage 5 items are
  recorded in §5 as closed by measurement.
- **Stage 6, G16 — every array `length` read paid the typed-array FFI probe
  (2026-09-28).** The census's `TypedArrayLength` row (15.7M, 16 rows) was
  labelled "typed-array `length`", which named the helper rather than the shape:
  it is the FFI probe at the end of `emit_member_cell_probe`'s `length` branch,
  and the compiled lowering sent the machine typed-array read's miss straight
  to it, ahead of the dense-Array probe. A machine read hits only a
  fixed/live/writable IntegerIndexed receiver, so a plain Array's `length`
  (the ubiquitous `i < a.length` loop bound) failed the machine gate, paid the
  helper, took its not-a-typed-array sentinel, and only then reached the dense
  probe. `opcost` alone showed 10.9M calls. The fix is a reorder, no new helper
  and no ABI change: the machine read's miss lands on the dense probe, the
  dense probe's miss lands on the FFI probe, and a non-Object receiver skips
  the FFI probe (it answers its sentinel for every non-Object unconditionally,
  so a String `length` no longer calls it). A/B on a plain 2e6-iteration
  `s += a.length` shape: **14.80 → 13.56 ms (−8.4%)**, `typed_array_length`
  14,000,000 → **0**. Corpus: `opcost` 10.9M → 0.7M, overall 17.47M → **0.7M**;
  `--jit-bench` in band (arithmetic 0.09, function calls 0.16, non-leaf call
  0.50, typed-array length 0.18). The length-bound probe is the wall-time
  evidence; the corpus rows that call the helper are bound elsewhere, so their
  row times are flat, which is the plan's "not row-only slices" caveat in the
  other direction — a general tax on a shape the corpus does not isolate. New
  `installed_jit_array_length_read_skips_the_ffi_probe` counts the helper
  through a wrapper (dense loop must never call it); detouring the dense hit
  through `ffi` counts 100,000 and fails (mutation-checked). Gates: fmt clean,
  clippy workspace `-D warnings` clean, workspace **5,543 passed / 0 failed**
  (`-p jit` 220), runtime `--no-default-features` 953/0, v8 `simdutf` 380/0,
  `cli --no-default-features --features jit` green, test262 `all`
  48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s, corpus
  parity 77 rows/0 mismatches (mean-jitGap 70.99 this run). No wasm sweep owed
  (`wasmtest` does not link `crates/jit`). Stage-6 re-measurement alongside it:
  the `for-in` head-let row is allocation-bound (952 JIT vs 1,058 jitless) and
  `arrays/for_of_dense` is already 1.52x under the JIT — both recorded in §5.
- **Stage 6, G17 — the compiled for-of fast-array cursor (2026-09-28).** The
  census's `ForOfNextBindLocal` (21.0M, one row) was the entire for-of advance:
  even the dense-array fast verdict paid a `call_slow` per element for
  `for_of_advance`'s two cache probes, so `arrays/for_of_dense` ran 38.8 ms
  against an equivalent inline index loop's 22.1 ms. The cursor now lives in
  **two hidden frame slots** (the `alloc_hoist_slot` mechanism), carried on
  `Step::ForOfBegin` as `cursor: (array_slot, index_slot)` (and on
  `Step::ForOfNextBindLocal`, `NO_FOR_OF_CURSOR` when the head has no fused
  bind): `for_of_begin` seeds the array slot with the dense-Array Value (or
  `undefined`), and the compiled `ForOfNextBindLocal` reads
  `elem_ptr[index]` from the live `array_dense` cursor — the invariants
  `emit_dense_element_read_into` already relies on (a live cursor, the index
  below `elem_len`, a non-hole element) — writes the loop slot, bumps the
  cursor and takes the back edge with no helper call and no value-stack round
  trip. The Vm's `for_of_stack` entry stays authoritative for the shared core:
  the first decline (a hole, the end, a receiver that stopped being dense)
  abandons the cursor (the array slot becomes `undefined`, so every later step
  takes the helper) and runs the new `for_of_fast_next`, which syncs the
  entry's index to the inline path's before advancing through
  `for_of_advance`. A **frame-slot** cursor is what makes it safe: it survives
  nesting (each loop owns its slots), recursion (each frame owns its slots)
  and suspension (the frame is saved), which a per-context cursor would not.
  The `Fixup::ForOfBoundary` resolve had to preserve the new field (a
  wholesale `ForOfBegin` reconstruction resets it — the same trap the bytecode
  skill records for `ForOfNext`'s `back`). Measured: `arrays/for_of_dense`
  **38.8 → 8.0 ms** (result byte-identical) with the row's
  `ForOfNextBindLocal` **21.0M → 0** and `ForOfFastNext` at one call per loop
  entry (the Done decline); the `arrays` family's `mean-jitGap` 28.72 → 21.54.
  Two new tests: `installed_jit_dense_for_of_inlines_the_cursor` (a wrapper
  count, mutation-checked — forcing the helper path counts 110,000 and fails)
  and `installed_jit_dense_for_of_matches_the_interpreter` (a checksum over a
  dense array, a middle hole, a trailing hole, a sparse array, a Set / String
  / TypedArray, a mid-loop grow and shrink, a `break`, nesting and a captured
  per-iteration `const`). Gates: fmt clean, clippy workspace `-D warnings`
  clean, workspace **5,545 passed / 0 failed** (`-p jit` 222), runtime
  `--no-default-features` 953/0, v8 `simdutf` 380/0, `cli
  --no-default-features --features jit` green, test262 `all`
  48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s, corpus
  parity 77 rows/0 mismatches, `--jit-bench` in band (arithmetic 0.08, function
  calls 0.15, non-leaf call 0.51). An `--gc-stress` for-of workload (a middle
  hole, a `push` per iteration) is byte-identical between jit and jitless, and
  a generator `yield`-ing inside a for-of exercises the saved frame cursor
  across a suspension. No wasm sweep owed (`wasmtest` does not link
  `crates/jit`). With this the Stage-6 iteration surface is closed for the
  fused `for-of`; G3, G5 and the non-fused `ForOfNext`/`ForInNext` remain.
- **Stage 6, G3 — the object literal: allocation-bound, and the shape walk
  removed (2026-09-28).** The plan's premise ("a helper sequence per
  element/step/property") was already false in the tree: Cut 72 fused
  `Step::ObjectFast` into ONE helper call. Measuring the residual (3M
  allocations per probe row, release, repeatable) split it: `{}` **53 ns**,
  `{a,b,c}` **84 ns**, `{a…h}` **114 ns** — so **54 ns is the fresh `JsObject`**
  (528 B initialized in `init_ordinary`; the three rows move ~1.6 GB), and
  ~9 ns per key is the rest. The literal is therefore ~64% fresh-object write
  bandwidth at the common 3-key shape, and no inline arm can touch it: that
  needs a smaller object or allocation sinking, both `crux`.

  The removable part is the per-key `Map::get_or_create_child` walk. `Map`
  gained a composite transition cache (`HashMap<Box<[AtomId]>, Handle<Map>>`,
  bounded at 1024, traced like `transitions`), so `object_fast_create`
  resolves the whole vector-free head in one lookup and records the shape on a
  miss — one hash + probe instead of N (a wash at 1 key, ~15 lookups saved at
  16), and the entries keep their shapes alive through `back_pointer`. Measured:
  `{a,b,c}` **84 → 74 ns (−12%)**, `{a…h}` **114 → 100 ns (−12%)**, `{}`
  unchanged, `objects/destructure` **176 → 166 ms (−5%)**. Corpus-wide this is
  inside the run-to-run noise (mean-jitGap 71.22 vs 68.35 over two runs, 0
  mismatches both) — recorded plainly, because the only literal-heavy corpus
  rows are `destructure` and `spread_assign`. New crux test
  `composite_child_caches_the_whole_literal_walk` pins the equivalence (the
  cached shape is exactly the walk's, keyed by the full list, and the walk does
  not populate the cache). Gates: fmt clean, clippy workspace `-D warnings`
  clean, workspace **5,546 passed / 0 failed** (crux +1), runtime
  `--no-default-features` 953/0, v8 `simdutf` 380/0, `cli
  --no-default-features --features jit` green, test262 `all`
  48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s, corpus
  parity 77 rows/0 mismatches, `--jit-bench` in band (arithmetic 0.07, function
  calls 0.16, non-leaf call 0.49). No wasm sweep owed (`wasmtest` does not link
  `crates/jit`; the change is `crux`+`runtime` and IS covered by both test262
  areas). The G3 residual is now recorded as a `crux` lever (object size /
  allocation sinking), not an inline path, so it is not re-opened here.
- **Stage 6, G5 — the template append: quadratic rebuild to a rope
  (2026-09-28).** G5 was mostly already landed (2026-09-07): the
  exactly-representable-integer `to_string` fast path and the fused
  string+primitive `Add` (`concat_primitive`, in `apply_binary` and the register
  executor's `binary_inline`), which took `coercion_concat` from ~320 to ~103 ms
  jit. `ConcatStrings` (string+string `+`) is already a direct helper, so it is
  a rope allocation and nothing else — string *building* had one quadratic
  shape left: the template append. Both the interpreter handler and the JIT
  helper rebuilt the accumulator flat per substitution (`string_units_of` →
  `Vec<u16>` → `from_utf16`), so a k-part template copied the whole accumulator
  k times — O(k·len), and an 18-substitution build loop measured 4.6 us per
  iteration. Both now call one shared `concat_template`/`concat_template_const`
  that appends with `JsString::concat` (a rope concat, O(k)), with the flat
  threshold preserving the leaf representation for short templates and
  `string_units_of` removed. Measured: 18-substitution build loop **929 →
  473 ms (−49%)**, 4-substitution **131 → 90 ms (−32%)**, and the real fixture
  `language/expressions/template-literal/evaluation-order.js` **371 → 336 ms
  (−9.5%)**. Content parity: a three-way jit / jitless / node check on an
  astral pair, a LONE surrogate and `s.slice(0)` agrees exactly (a lossy UTF-8
  round trip would have shown U+FFFD), and the new
  `installed_jit_template_rope_matches_the_interpreter` folds a
  12-substitution rope build plus `s === s.slice(0)` and `s === f(...)` into
  one checksum against the interpreter — mutation-checked by making the JIT's
  `concat_str_const` append a wrong constant, which fails it. Gates: fmt clean,
  clippy workspace `-D warnings` clean, workspace **5,547 passed / 0 failed**
  (`-p jit` 223), runtime `--no-default-features` 953/0, v8 `simdutf` 380/0,
  `cli --no-default-features --features jit` green, test262 `all`
  48,464/0/0/0 of 48,622 and `intl402` 3,205/0/0/0 of 3,357 at 15s/15s, corpus
  parity 77 rows/0 mismatches, `--jit-bench` in band (string concat 0.17,
  buildString shape 0.20, buildString full 0.26). The `strings` family's
  remaining gap to node is the builtin-call path (a method read + `CallSlow`
  per `s.charCodeAt`), not string building, and belongs to G10/CallSlow.
  No wasm sweep owed (`wasmtest` does not link `crates/jit`; the change is
  `runtime` and IS covered by both test262 areas).
- **Stage 6 closed — the non-fused `ForOfNext` / `ForInNext` measured, not
  landed (2026-09-28).** Stage 6's last item was the two protocol steps G17
  left on the helper. Measuring them first (the plan's own rule) closed both:

  `ForOfNext` (the stack variant, the census's 21.0M was the *fused*
  `ForOfNextBindLocal` that G17 inlined) now shows **0 corpus calls** — it is
  emitted only for a for-of head bound to a CONTEXT slot, i.e. only when a body
  closure captures the head, which also emits `EnterPerIteration`/`PerIteration`
  (a spec-required fresh env plus a closure per iteration). That shape is
  allocation-bound for the same reason the for-in head-let row is, so the
  advance cannot pay; the G17 cursor is allocated for every certified for-of, so
  extending it would be cheap, but there is no measured shape behind it.

  `ForInNext` is 9.1M over `control/for_in.js`, and the decomposition shows the
  compiled step is already within ~3.5% of the interpreter: the `fast` verdict,
  the generation-skip per-key check and the `ForInBegin` enumeration cache all
  landed 2026-09-06 (`.notes/perf.md`), so the row is 34.9 ms jit / 49.7 ms
  jitless. Isolated (150k re-entries of a 5-key loop, min-of-reps) a for-in
  with no computed read is **8.53 ms jit** against an equivalent plain
  `for (i = 0; i < 5; i++) n += 1` loop at **1.82 ms** — the whole protocol is
  6.7 ms — while the interpreter's protocol is 5.5 ms (19.03 − 13.56). The
  compiled step's deficit is therefore **the helper call alone, ~1.2 ms of a
  34.9 ms row (~3.5%)**, and inlining it would need a five-slot frame cursor
  (keys pointer/length/index, base pointer, generation) plus a
  machine-readable `keys` element layout. The row's real cost is elsewhere: an
  `o[k]` computed read on a non-Array object is ~35 ns of the 46.5 ns step
  (`GetMemberComputed` 5.25M in the same directory), which is a per-site-IC
  surface — **Stage 7** (landed as G8, below), not an inline arm. Recorded so
  neither is re-opened as one. No code changed (the plan's method is exactly
  this: falsify by measurement before investing).
- **Stage 7, G8 — the computed read's key cell, and the store discipline that
  falsified the first cut (2026-09-28).** The census's `GetMemberComputed`
  (73.4M, 12 rows) was the whole helper, not the interning: the same read
  *named* is 7.0 ns with zero helper calls (machine code), *computed* was
  35.1 ns with a helper per read, and neither key length (`str_key_30.js`
  54.7 ms, barely above the 30-char-free 52.7) nor key kind (`int_key.js`
  61.1) moved it — so the cost was the helper body (nullish check,
  `to_property_key`/intern, `member_cell_get`, key compare), not collisions.
  The first design cached the **value** in a cell keyed by the key Value's
  identity so the compiled probe could run before the key was converted: it
  measured `str_key.js` 52.7 → **9.86 ms** and looked decisive, but it was
  wrong. An in-place value write (`JsObject::write_data_property_slot`, the
  JIT's `SetMemberSlot`) deliberately does NOT bump the receiver's generation —
  it refreshes `member_value_cells` instead (see the `set_key`/
  `write_data_property`/`write_data_property_slot` notes) — so a value cell
  that validates only the generation served a stale read. The corpus caught it
  at once: **59 `harness` fixtures failed** (`verifyProperty`'s
  read/write/restore of a computed key), reproduced minimally by
  `o["a"] = 1; o["a"]; o["a"] = 2; o["a"]` answering `1`. The landed design
  keeps the key cell as a **box → atom map** (both sides immutable — strings
  are immutable and the interner is append-only — so it never needs
  invalidation) and takes the VALUE from `member_value_cells`, the cell every
  store path already maintains, so a hit is exactly the interpreter's own
  `member_cell_get`. The compiled probe is three branchy blocks in
  `emit_element_read`: the receiver's Object tag (already the dispatch gate),
  `is_string(key)`, the key cell's box compare, then `(live_id ^ atom) &
  (MEMBER_CELLS-1)` validated on id/name/generation. Results: `str_key.js`
  **52.7 → 10.6 ms** (helper 13: 31.5M → 35, warmup only), `str_key_30.js`
  54.7 → 10.65, and the real-world `control/for_in.js` **34.9 → 10.54 ms jit**
  (26.9 jitless) — the row's `o[k]` cost, now inline. `named.js` (0.23) and
  `var_named.js` (11.3) are unregressed; corpus parity is 0 mismatches over 77
  rows. Number-keyed `o[i]` is **not** served — the cell is String-keyed by
  design, matching the interpreter's atom conversion — so `int_key.js` reads
  ~68 ms, still 1.8× jitless, paying the added `is_string` gate on the unserved
  path. Tests: `installed_jit_computed_read_cell_inlines` (the helper runs only
  while the cell warms; mutation-checked by breaking the emitter's slot
  arithmetic → 100,000 calls), `installed_jit_computed_read_after_inplace_store_matches_the_interpreter`
  (the store-visibility contract the falsified design broke; mutation-checked),
  and `the_emitter_and_runtime_computed_read_slots_agree`. Gates: fmt, clippy
  `-D warnings`, workspace tests (test262 3,324, runtime 984, jit 226), runtime
  `--no-default-features` 953, `v8 --features simdutf` 380, cli
  `--no-default-features --features jit` check, test262 `all` 48,464 pass /
  0 fail / 0 crash / 0 hang (158 skip), `intl402` 3,205 pass / 0 fail (152
  skip), `--jit-bench` all `result-ok`, corpus parity 77 rows 0 mismatches. No
  wasm sweep is owed: `wasmtest` does not link `crates/jit`.
- **Stage 7, census re-rank — G8 verified, `LoadContext` closed by representation,
  and a build trap that skews the census (2026-09-28).** Two findings.

  **The census binary matters.** `cargo build --release -p cli -p test262`
  UNIFIES `crux/typed_array::WORKERS` ON (test262 pulls `runtime/workers`), and
  the compiled typed-array read/store lanes deliberately decline when WORKERS is
  on (`emit_typed_array_read_into` jumps to `legacy` under `if
  crux::typed_array::WORKERS`), so that binary measures the interpreter's element
  lanes. The same row proves it: `arrays/typed_array.js` is **59.8 ms with the
  unified binary and 30.3 ms with `-p cli` alone**, its helpers dropping from
  14.0M `FastArrayElementWrite` + 14.0M `GetMemberComputed` to **zero**. Measure
  the corpus with `-p cli` ALONE; only the *sweep* binaries come from
  `-p cli -p test262` (the unification does not affect conformance, only timing
  and the census).

  **On the correct binary** the census is `GetMemberName` 96.1M (G10 — measured
  and reverted: warm chain reads are fixed-cost), `SwitchTest` 91.9M +
  `SwitchDisc` 21.0M + `BreakControl` 21.0M (G11 — closed: already 3.1× jitless),
  `CallSlow` 90.9M (G2's non-leaf residue), `SetMemberSlot` 70.0M (~1 ns/iter,
  closed), `BinarySlow` 43.7M (G9 landed the six bitwise/shift inlines; this is
  the guard-fallback residue the plan already deferred), `LoadContext` 38.8M
  (`calls/closure_capture.js` 14.0M, `recursive_fib.js` 14.9M, `language` 8.5M,
  `control` 1.4M; `closure_capture` is 40.7 ms jit vs 134.3 jitless). **G8's
  residual is under 2.1M** (not the 14.3M the skewed binary showed — that was the
  typed-array artifact), and `FastArrayElementWrite` is only 2.24M while
  `SetMemberComputed` is 6.2M, 77% of it the single small
  `opcost/element_write.js` row (1.84 ms). The dense-array overwrite
  (`arr[k] = v`, index below the length — the one store shape the append arm and
  the typed-array store both decline) is therefore a **real but small** target,
  contained and worth doing on its own merits, not the 16.2M the skewed census
  implied.

  `LoadContext` is **closed by representation, not by measurement**: a machine
  read would have to walk `context_chain_env` (a `depth` count that skips
  context-TRANSPARENT envs, so the walk length is not the static `depth`), through
  `EnvRef`'s Rc indirection, check the `EnvRecord` variant, then index
  `DeclarativeEnv::bindings: RefCell<Vec<(JsString, Binding)>>` — a borrow flag, a
  tuple stride, and an enum-carrying `Option<Value>` TDZ marker. Inlining it would
  be a `crux` env-model change (a flat, machine-addressable slot array), which is
  out of this plan's scope; the same shape that makes `member_value_cells` usable
  (a flat `#[repr(C)]` array on the Agent) is exactly what the env lacks. The
  remaining call-side budget is **the non-leaf call** (`recursive_fib` 289.5 ms
  jit vs 471.1 jitless — the largest single gap at 181.6 ms — and `--jit-bench`'s
  `non-leaf call` ratio 0.51 on the corrected binary, the weakest arm), plus
  `LeafCallFill` 23.2M and `FastArrayElementWrite` 2.24M. No code changed.

  Suggested .rules additions for this session's PR: check `git log --oneline -3`
  and `git status --short` before editing (the operator commits between turns, so
  a file you edited may already be in HEAD); and `grep -c` exits non-zero on a
  zero count, silently breaking `&&` chains.
- **Stage 7, G8 follow-up — the key cell's index collided (2026-09-28).** The
  landed G8 index was `key_bits & 63`, which reads only the box address's low
  bits — 64 slots from six address bits. The suite exposed it: the
  `installed_jit_computed_read_cell_inlines` bound (`< 100` helper calls) failed
  about one `-p jit` run in six with **40,003** calls — two of the five key boxes
  aliasing into two slots, so 2/5 of the reads missed to the helper (the box
  addresses depend on the allocation history, which the parallel suite shifts).
  The index is now a multiplicative hash (`COMPUTED_READ_INDEX_MUL`/`_SHIFT`,
  the `leaf_record_slot` discipline, mirrored by the emitter's
  `emit_computed_read_slot` and the agreement test); six consecutive full
  `-p jit` runs green and `str_key.js` 10.9 ms with 35 helper-13 calls
  (unregressed). Recorded because the same low-bits index pattern is the obvious
  thing to reach for in a new direct-mapped cell. The hash fixed the OUTER
  cell's aliasing (a real improvement), but a second aliasing cause remained:
  the probe takes its VALUE from the 16-slot `member_value_cells`
  (`(id ^ atom) & 15`), so a five-key object aliases there in roughly 30% of
  layouts — and *which* atoms the keys get depends on the process-global
  interner's state, i.e. on what ran first in the parallel suite, which is why
  the count test kept failing intermittently at a lower rate. Addressed with
  G19 (the count test is now a one-key shape; the many-key shape is
  differential) — see that entry.
- **Stage 7, G19 — the `switch` discriminant and case test in machine code
  (2026-09-28).** G18 left the row's `SwitchDisc` 21.0M + `SwitchTest` 91.9M,
  both helper calls for what is a store and a compare. The blocker was storage:
  `Vm::switch_disc: Option<Value>` is not machine-readable (Rust's `Option`
  layout, an unaddressable discriminant), so the field became a raw `Value` plus
  a `switch_disc_set: bool`, with `VM_SWITCH_DISC_OFFSET`/
  `VM_SWITCH_DISC_SET_OFFSET` exported like the other `VM_*` fields (a
  *register* was rejected on purpose: a case expression can suspend, and only
  Vm/frame state survives a resume). The compiled `SwitchDisc` writes the bits
  and the flag with two stores; the compiled `SwitchTest` loads the bits and
  compares inline with `===`'s three fast paths — two Numbers via one f64
  compare (`NaN` unequal, `+0 === -0`), otherwise identical bits (the same
  object/symbol/string box/BigInt box/bool/null/undefined) — falling to the
  `switch_test` helper for distinct string/BigInt boxes with equal content and
  kind-mismatched tests, which reads the same field. `control/switch_dispatch.js`
  **42.2 → 23.7 ms jit** (210.8 jitless; the row is 63.5 → 23.7, **8.9×**), with
  `SwitchDisc`/`SwitchTest`/`BreakControl` all at **0** corpus calls (only
  `GcSafepoint` remains). Tests: `installed_jit_switch_disc_and_test_inline`
  (both counters; mutation-checked → 100,000 `switch_disc` calls),
  `installed_jit_switch_mixed_case_falls_to_the_helper` (a kind-mismatched case
  must take the helper),
  `installed_jit_switch_edge_values_match_the_interpreter` (differential over
  `0`/`-0`/`true`/`null`/`undefined`/`'x'`/`1`/`NaN`), with the existing
  switch-in-try and hot-switch-loop tests. Recorded limitation: the
  computed-read probe's coverage is capped by `MEMBER_CELLS`, unlike the named
  read, which also has the map/shape fallback — a follow-up if a many-key hot
  object shows up. Gates: fmt, clippy `-D warnings`, workspace (test262 3,324,
  runtime 984, jit 232), runtime `--no-default-features` 953, `v8 --features
  simdutf` 380, cli `--no-default-features --features jit`, test262 `all` 48,464
  pass / 0 fail / 0 crash / 0 hang, `intl402` 3,205 pass / 0 fail, `--jit-bench`
  all `result-ok`, corpus parity 77 rows 0 mismatches, 12 consecutive `-p jit`
  runs green.
- **Stage 7, G20 — the dense-array element overwrite in machine code
  (2026-09-28).** The compiled computed store had exactly two element arms (the
  dense APPEND and the typed-array store), so `a[i] = v` with `i` below the
  length — an in-place overwrite over an already-filled array — fell to the
  general helper on every store. The census put `SetMemberComputed` at 6.2M
  (77% of it the one `opcost/element_write.js` row) and
  `FastArrayElementWrite` at 2.25M. The new `emit_dense_element_store_into` arm
  is wired as the typed store's miss target in BOTH the step
  (`AssignMemberComputed`) and register (`emit_computed_store`) tails, so the
  helper chain is append → typed → overwrite → helper. Its gates mirror the
  READ arm (the packed Object tag in one compare, the f64 round-trip
  canonical-index test, `idx < elem_len`, the non-hole element) plus the
  APPEND's write discipline: while dense every element is a writable data
  property that shadows the whole chain (no attribute or chain check), a
  registered prototype declines (the store's generation bump advances the
  elements protector, which machine code does not), and the store stays inline
  only when no write barrier can be needed (`slots_young || !value_is_heap`).
  The write is `elem_ptr[idx] = value` plus the generation bump — no length or
  cursor change, exactly the interpreter's `array_element_write_dense`
  in-place branch, so the generation-keyed element/length cells invalidate as
  they would there. Measured: `opcost/element_write.js` **1.84 → 1.17 ms**
  (−36%) and the hot loop's `SetMemberComputed` **gone** (the row's remaining
  `FastArrayElementWrite` is the `new Array(1024)` fill's *hole* fills, which
  decline correctly). Tests: `installed_jit_dense_element_overwrite_inlines`
  (counts both store helpers; mutation-checked via the prototype gate →
  100,000 `set_member_computed` calls) and
  `installed_jit_dense_element_overwrite_declines_match_the_interpreter`
  (differential over a grow/hole-fill, a registered prototype, and heap object
  values — the barrier gate). Gates: fmt, clippy `-D warnings`, workspace
  (test262 3,324, runtime 984, jit 234), runtime `--no-default-features` 953,
  `v8 --features simdutf` 380, cli `--no-default-features --features jit`,
  test262 `all` 48,464 pass / 0 fail / 0 crash / 0 hang, `intl402` 3,205 pass /
  0 fail, `--jit-bench` all `result-ok`, corpus parity 0 mismatches. Measurement
  caveats recorded: the FIRST corpus run after a build is cold (`recursive_fib`
  read 798 ms once against a warm 300 ms), and the corpus parity mean is
  load-sensitive — compare the `jitless` column too (it moved 6.45 → 6.88 in
  the same run the mean moved, which is machine load, not a regression).
- **Stage 7, G18 — a compiled `break`/`continue` jumps directly when no finally
  or for-of is in scope (2026-09-28).** The census's `BreakControl` (21.0M, the
  one `control/switch_dispatch.js` row) was the whole compiled `Step::Break`:
  `emit_dispatch_call` ran the `control_transfer` helper AND `bump_leaf_epoch`
  (the call-site comment justified the bump as "the dispatch mutates the
  try/pending/env stacks the leaf-call probe reads"). But for a `Break` with no
  try frame and no for-of, `control_transfer` is exactly `self.ip = target`:
  `aborts_pending` needs a pending finally, the finally loop needs a try frame,
  and `close_for_of_upto` needs a for-of boundary — it pops the *async* stack
  too, so a body with `AsyncForOfBegin`/`Next` must be excluded. The gate is the
  Lowerer's existing `has_try`/`has_for_of` plus a new `has_async_for_of`
  (computed beside them in `Lowerer::new`); when all three are false the arm
  emits `jump(ensure_block(target))` — the `Step::Jump` idiom and nothing more.
  Measured: `control/switch_dispatch.js` **63.5 → 42.2 ms jit** (219.3 jitless;
  3.45× → 5.2×) with `BreakControl` **21.0M → 0**. The row's remaining helpers
  are `SwitchDisc` 21.0M + `SwitchTest` 91.9M — **G19**, the discriminant and
  per-case test inline (the discriminant lives in `vm.switch_disc:
  Option<Value>`, which machine code cannot read, so G19 needs a machineable
  home — a hoisted frame slot or a `Value` + presence-flag pair — plus an inline
  strict-equality for the case test; the case values are usually literal
  constants, and a Number/identity fast path with a helper fallback for
  strings/BigInts is the shape). Tests:
  `installed_jit_direct_break_skips_the_control_helper` (helper counted;
  mutation-checked → 100,000 calls) and
  `installed_jit_break_through_a_finally_still_uses_the_control_helper` (the
  negative gate), with the existing
  `installed_jit_for_of_break_and_return_close_the_iterator` and
  `installed_jit_labeled_break_out_of_an_acc_loop_syncs_the_counter` guarding
  the for-of/labeled cases. Gates: fmt, clippy `-D warnings`, workspace tests
  (test262 3,324, runtime 984, jit 228), runtime `--no-default-features` 953,
  `v8 --features simdutf` 380, cli `--no-default-features --features jit`, and
  after the G8 index fix a re-run of the measurement set: test262 `all` 48,464
  pass / 0 fail / 0 crash / 0 hang, `intl402` 3,205 pass / 0 fail, `--jit-bench`
  all `result-ok`, corpus parity 77 rows 0 mismatches (`mean-jitGap` 87.0 →
  67.8).
- **Stage 7, G21 — the compiled self-call (2026-09-28).** The last "real" call-side
  deficit: `recursive_fib.js` at ~290 ms jit vs ~518 jitless and `--jit-bench`'s
  `non-leaf call` at 0.52 (the weakest arm). The design pass falsified the
  obvious premise first: `fib`/`bench` DO compile and run compiled —
  `ordinary_call` -> `run_compiled_body` -> `run_jit_body` is the general
  activation funnel, so a non-leaf body is not interpreted wholesale — but each
  recursion goes `emit_call` -> `Helper::CallSlow` -> `do_call_fast` ->
  `ordinary_call` -> a fresh Vm take/return, an execution-context push, a frame
  setup, a global-shadow walk, a root registration, a new `JitCallContext` and a
  new working buffer, per level. The slice moves the fast path into the runtime:
  `call_slow` (`crates/runtime/src/jit.rs`) checks `callee ==
  ctx.current_function` (all 64 NaN-box bits, so a self-call's callee IS the
  running closure and its body is this same compiled body) and, when the body is
  `self_call_eligible` and `agent.jit_depth < MAX_JIT_DEPTH` and no termination
  is pending, runs the compiled entry directly (`self_call_inline`): a nested
  frame carved from a private per-call buffer, the params/TDZ/vars filled
  (`run_leaf_body`'s layout, with the gate guaranteeing no `this`/`arguments`
  slot and no per-call capture context, so the nested run shares the caller's
  environment unchanged), the buffer rooted via `vm.jit_roots`, and `ctx.buf_end`
  plus the array-index cursor swapped to the nested buffer for the call and
  restored after. Doing it in the runtime (not the emitter) is what keeps the
  slice small: the helper funnel already receives the shared ctx and the
  argument region, so there is no new helper, no four-file mirror and no
  Cranelift change, and the fallback is the unchanged interpreter path.
  `CompiledBody::self_call_eligible` (`crates/runtime/src/ir.rs`) is the gate:
  certified, no try/for-in/for-of/async-for-of/destructuring/suspension step, no
  `CreateArguments`/`NewTarget`, no `CallApply`, `context_names` empty and no
  `this`/`arguments` slot; `run_jit_body` takes the verdict as a parameter
  (true only from `run_compiled_body`, false from the script/async/generator
  drivers, so a resumable body can never take it). Measured on the `-p cli`-alone
  binary: `recursive_fib.js` **~290 -> ~43 ms jit** (stable 42.0/42.8/43.8/43.3
  over four runs; 14.7x jitless, `jlGap` 14.72), `--jit-bench` `non-leaf call`
  **0.52 -> 0.40** (no longer the weakest arm; `compound assign` 0.50 is) with
  every other arm in band and nothing regressed. The census is intentionally
  unchanged (the path lives INSIDE the helper, so `call_slow`'s count is the
  same; the wall clock is the evidence). Tests: `installed_jit_a_self_recursive_
  call_matches_the_interpreter` (jit-vs-jitless differential over three shapes +
  the absolute answers 4800/7660/800 + `compiled >= 2`; mutation-checked by
  zeroing the param fill → the differential fails) and
  `installed_jit_a_deep_self_recursion_falls_back_past_the_depth_cap` (depth 400
  past `MAX_JIT_DEPTH` on a large stack computes correctly, and a runaway
  recursion still surfaces the interpreter's catchable `RangeError` under both
  modes). Recorded limitations: the nested activation pushes no
  `ExecutionContext` (so a stack trace loses self-recursive frames — exactly as
  the leaf-inline and `TailCallSelf` paths already do), and only self-calls take
  it (a non-recursive call to another certified function still runs the full
  funnel). Phase 2 (recorded in §3): a machine-code `call_indirect` lane that
  removes the FFI round trip, and the general non-self callee (which needs the
  callee's scope/entry at the call site). Gates: fmt, clippy `-D warnings`
  (workspace, all-targets), workspace tests, `-p jit --lib`, `-p runtime --lib`
  (981), `-p runtime --no-default-features --lib` (953), `-p v8 --features
  simdutf --lib` (380), `-p cli --no-default-features --features jit`, test262
  `all` 48,464 pass / 0 fail / 0 crash / 0 hang of 48,622, `intl402` 3,205 pass /
  0 fail of 3,357, corpus parity 77 rows 0 mismatches, `--jit-bench` all
  `result-ok`. No wasm sweep owed (`wasmtest` does not link `crates/jit`, and
  `crux` is untouched).
- **Stage 7, G21 Phase 2a — the emit-side self gate, plus a retraction and two
  measured next-levers (2026-09-28).** A call site in a scoped body now loads
  `JitCallContext::current_function`, compares the resolved callee against it
  (all 64 NaN-box bits, plus a non-zero check so a script's `0` never matches),
  and branches straight to the `call_slow` lane — skipping the ~15-20
  instruction leaf-record gate entirely. That gate could only ever *refuse* a
  self callee (the running body contains this call step, so it is not a leaf),
  and the branch is always not-taken at a non-self site, so the emit-side tax
  is below measurement (`direct_leaf` 12.4 ms, unchanged). `slow`/`merge`
  hoist to the top of `emit_call`; the gate is emitted only for a scoped body
  (`self.scope.is_some()`), since a script's `current_function` is 0. Measured:
  `recursive_fib.js` **~43 -> ~39.7 ms** (stable 39.68/39.78/39.83 over three
  runs), every other row in band. **Retraction:** the Phase 1 entry's
  `--jit-bench` `non-leaf call` `0.52 -> 0.40` claim above is withdrawn — that
  row is `bench(n){ ...mid(i)... }` with `mid(x){ leaf(x)+1 }` and
  `leaf(x){ x+1 }`, i.e. a **non-self** call, and its *jit* time never moved
  (13.87 vs 13.82 ms); the ratio moved with the **interpreter** column, which
  is load-sensitive (26.9 vs 34.9 ms across runs). The evidence Phase 1 owns is
  `recursive_fib`'s **jit** wall time, and that one is solid. **Two next-levers
  measured, both for Phase 2b, both blocked or deferred for stated reasons:**
  (1) the self path's per-call `[Value; 64]` buffer + `vm.jit_roots` push/pop is
  **~11%** — bounding the buffer at 24 slots measured `recursive_fib` at
  37.6-39.7 ms — but a static bound of that size heap-spills for any body whose
  `frame_size + stack_usage > 8`, i.e. a malloc per recursive call for a
  real-looking walker, so a smaller const is **not** safe; the safe fix is a
  per-run/per-agent frame arena rooted once, and that is blocked by `crux`'s
  tracing of a **stale** (freed-slot) `GcAny` — a reused arena slot can hold a
  dead box's bits, whose stale vtable walk is unsound — so the arena needs a
  release-time clear (which is the same init cost) or a crux-side "is this
  handle live" test before it can land. (2) the general non-self call is the
  real lever: `--jit-bench`'s `non-leaf call` is **~140 ns/call**
  (~13.9 ms jit for 100k, against ~6.2 ms jitless and the ~35 ns of the
  hand-written `array_for_each_js` control), all of it the
  `ordinary_call`/`run_compiled_body`/`run_jit_body` funnel. Gates: fmt; clippy
  `-D warnings` (workspace, all-targets); `cargo test --workspace` green;
  `-p jit --lib` 236; `-p runtime --no-default-features --lib` 953; `-p v8
  --features simdutf --lib` 380; `-p cli --no-default-features --features jit`;
  test262 `all` 48,464 pass / 0 fail / 0 crash / 0 hang of 48,622; `intl402`
  3,205 pass / 0 fail of 3,357; corpus parity 77 rows 0 mismatches; `--jit-bench`
  all `result-ok`. No wasm sweep owed.
- **Stage 7, G21 — the Phase 1 self path corrupted a caller on a strict tail
  base case; fixed by widening the eligibility gate (2026-09-28).** Found while
  beginning Phase 2b: a self-eligible body whose base case is a **strict tail
  call** ("use strict"; `function f(n) { if (n <= 0) return g(7); return
  f(n-1)+1; }`) returned **10714 where the interpreter returns 11200**. Root
  cause: the nested self run shares the caller's Vm, and a `TailCall*` step
  calls `Helper::TailCall` -> `tail_prepare_ordinary`, which swaps `vm`'s
  frame/context for the tail callee and sets `ctx.tail`; `self_call_inline`
  ignored both, so the outer body resumed on a Vm reset for another body and
  pushed the tail helper's placeholder as the call result. The bug is in the
  committed Phase 1 (`212a6aa5`), and the phase-1 gates did not catch it because
  no differential shape exercised a strict self-recursive body with a tail base
  case. **Fix:** `CompiledBody::self_call_eligible` (`crates/runtime/src/ir.rs`)
  now also excludes every step that runs user code while building caller-owned
  Vm state, or mutates that state itself — all `TailCall*`/`TailCallSelf*`
  variants, `ArgsBase`/`ArgsPush`/`ArgsSpread`, the vector `Call`, `Construct`,
  `TaggedTemplate`/`TailTaggedTemplate`, and `SuperCall`. Bodies that hit one of
  these fall back to `CallSlow` (correct, just not fast); `fib` has none and
  keeps the fast path (**38.4-39.3 ms**, unchanged). **Evidence:** the new
  `installed_jit_a_self_recursion_with_a_tail_base_matches_the_interpreter`
  (differential + absolute 11200) and
  `installed_jit_self_call_hazards_match_the_interpreter` (a spread call, a
  string builder, and a statement-position fused call-store inside the self
  body; differential + absolutes 1100/1200/500). Mutation-checked: removing
  `Step::TailCallFast` from the gate reintroduces the 10714 result, so the gate
  is the fix rather than a coincidence. Rendered live in release: a scratch
  corpus row reads 11200 in both modes. **Design consequence for 2b:** the
  shared-Vm/ctx assumption is the hazard source; a general compiled call must
  run the callee on a **pooled private Vm** and carry a cached per-callee
  descriptor (entry/scope, closure `environment`, `globals_unshadowed`), none of
  which is available to Phase 1's shared-ctx shape (see §3). Gates: fmt; clippy
  `-D warnings` (workspace, all-targets); `cargo test --workspace` green;
  `-p jit --lib` 238; `-p runtime --no-default-features --lib` 953; `-p v8
  --features simdutf --lib` 380; `-p cli --no-default-features --features jit`;
  test262 `all` 48,464 pass / 0 fail / 0 crash / 0 hang of 48,622; `intl402`
  3,205 pass / 0 fail of 3,357; corpus parity 77 rows 0 mismatches. No wasm
  sweep owed.
- **Stage 8 (G21 Phase 2b) design written, no code changed (2026-09-28).** §5's
  Stage 8 is the general compiled call — a call to a *different* certified
  function, still the ~140 ns/call funnel (`--jit-bench`'s `non-leaf call`, jit
  ms). It is staged as **8.0** (measure the funnel's components before choosing
  a slice — hash, `new_body_context`, context push/pop, `take_vm`/`return_vm` +
  `Vm::reset`, frame setup, globals walk, ctx build, work-buffer init,
  `with_jit_run`), **8.1** (a per-callee `Agent` descriptor holding the
  per-closure-invariant entry/scope/`globals_unshadowed`/intrinsic bits, with
  `Function::environment` read off the callee rather than cached — landable on
  its own and re-measured), and **8.2** (a private-Vm lane that runs the callee
  compiled on a pooled Vm with a fresh ctx, handling `ctx.tail` by looping). The
  design's hard constraint comes from this arc's own bug: the callee must not
  share the caller's Vm or ctx (§8's fix entry, skill §20.3). Acceptance and
  traps (sweep-clear the descriptor, register the pooled Vm as an active run,
  preserve the `ExecutionContext`/`dispose_env_resources` fidelity the general
  call owes, and treat `Vm::reset` as a possible wash) are in §5. No code
  changed.
- **Stage 8.0 — the general-call funnel measured, and the result cancels 8.1/8.2
  (2026-09-28).** Three temporary probes (all removed; the tree is byte-identical
  above them apart from this record). **Per-op costs (release micro-bench):**
  `take_vm`+`return_vm` incl. `Vm::reset` **12.8 ns**, `ExecutionContext`
  push+pop **14.9 ns**, `global_reads_are_unshadowed` **2.4 ns**,
  `ecma_functions.get` hit **0.8 ns**, `JitCallContext` build **3.0 ns**, TLS
  push/pop **1.4 ns**. **Region timers** put the whole `call_slow` interpreter
  path at ~138 ns/call (the row's own jit time) with the emit-side probe and
  helper entry minor, and the remainder (~100 ns) diffuse across the
  `do_call_fast`/`call_inner`/`ordinary_call` dispatch and
  `run_compiled_body`/`run_jit_body`'s setup — no single dominant item. **The
  finding:** the two largest single costs are the per-call Vm take/return+reset
  and the `ExecutionContext` push/pop, both **structural to running each JS call
  on its own Vm and execution context** and both **kept** by a private-Vm lane;
  what 8.1/8.2 could remove (hash, globals walk, intrinsic read, some dispatch)
  is ~10-15 ns of ~138, ~10%, for a large hazardous lane. **And 8.2's own shape
  is a wash:** sharing the caller's Vm needs a ~15-field per-frame save/restore
  (10-15 ns) that costs what the 13 ns pooled take/reset it removes costs — so
  the fix is not to share the Vm but to make a frame a compact stack-allocated
  record on one Vm (**§5 Stage 9**), and to find the ~100 ns the region timers
  could not attribute before redesigning `Vm` at it. So §5's Stage 8 is
  **closed by measurement**: 8.1 is not worth a slice, 8.2 is not to be built as
  designed, and the architectural change it pointed at is written up as Stage 9
  (the one-Vm frame-record redesign). No code changed. Gate note: the temporary
  probes touched
  `crates/runtime/src/jit.rs` and `crates/runtime/src/function.rs` and were
  fully removed (both files match HEAD in content); only the plan differs.
- **Stage 9, Cut 0 the call-side profile (2026-09-29).** An `rdtsc`
  exclusive-time profiler over 14 regions on the release `non-leaf call` row
  replaced the ablation plan (timing-only, removed again — `git diff crates/`
  empty above it). Result: the row decomposes into a **chain of ~14 comparable
  pieces, none over 14.4%**, not one ~100 ns item; `mid` is compiled and no
  interpreter fallback runs in steady state (the `jit_code` entry count equals
  the call count, `vm.start` ran 16 times). The two structural clusters are the
  interpreter round-trip (`do_call_fast` 24.9 + `complete_pending` 27.6 +
  `ordinary_call` 28.1 instrumented) and the pooled-Vm round trip (`take_vm`
  8.1 + `return_vm` 18.8, both pure). This **revises Stage 9's Cut 1 shape**:
  the lane is the self path's — a private frame/work buffer plus a per-callee
  `JitCallContext`, entered from `call_slow`, saving only the handful of `Vm`
  fields the callee reads — not a 15-field `FrameRecord` on `Vm::frames`
  (that figure described sharing `vm.frame`). Numbers and the argument: §5
  Stage 9's Cut 0 bullet. Only the plan differs from HEAD.
- **Stage 9, Cut 1 the certified-callee lane (2026-09-29).** `call_slow` runs a
  call to a *different* certified body's compiled entry directly (a nested
  frame in a private buffer, its own `JitCallContext`) instead of the
  interpreter funnel + pooled Vm. `Vm::save_scratch`/`restore_scratch` isolate
  the per-activation registers; the lane saves/restores the five control
  fields. **Measured A/B in one tree: `--jit-bench` `non-leaf call` 14.59 →
  5.40 ms (ratio 0.54 → 0.21), 2.7×.** New gate `CompiledBody::
  private_frame_safe` (a `vm.frame`-reading helper — the `s += e` builder, a
  hoisted block function declaration — cannot run on a private-buffer frame),
  and `context::resolve_binding_from` (compiled binding reads resolve from
  `vm.lexical_env`, not the caller's `ExecutionContext`). A new differential
  test (`installed_jit_a_certified_callee_matches_the_interpreter`) covers a
  non-self callee with a `switch` under a fall-through caller, mutual
  recursion, and a global-reading callee. Gates: fmt/clippy clean; workspace
  green (`jit --lib` 239, `runtime --lib` 981, `crux` 260, `test262` 3324,
  `v8` 372); test262 `all` **48,464 pass / 0 fail / 158 skip of 48,622** and
  `intl402` **3,205 / 0 fail / 152 skip of 3,357** (both exactly baseline);
  the eight wasm `run --strict` suites 20,662 + 25,990 + 77 + 7,485 + 105 +
  654 + 8,709 + 912 = **64,594 checks, 0 fail / 0 pending**, JS-API **1,001 /
  0 fail**; the corpus **77 rows / 0 mismatches**; no `--jit-bench` row
  regressed. The `resolve_binding_from` change is additive and
  behavior-preserving for the funnel path (the two envs are equal there), which
  is why the sweeps reproduce the baseline exactly.
