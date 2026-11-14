# The register tier: values in registers, inlined, rooted

**Status: plan, revised 2026-11-14 against `a84643bc` + the B0 census.** The
earlier revision of this document (same date, morning) was written against the
2026-10-09 lift-coverage snapshot and is now wrong about the tree in a
load-bearing way: the fused `for` loop, the literal creates, the typed `Number`
lane, trial inlining and literal scalar replacement have all landed since, so
the tier *does* run on the corpus. §1 is re-measured, §3 is a measured coverage
census, §4 is the actual remaining work. §2 (the value model), §6 (the protocol)
and §7 (the traps) stand, with §2.2 and §7 corrected where the code disagrees.

Companion documents: `optimizing-tier-plan.md` (the strategy: specialize and
retire, never deopt), `optimizing-tier-impl.md` (what is landed and what each
increment owes), `opt-int-representation.md` (the `Int` lane, the template for
the `Float` lane), `opt-per-op-overhead.md` (the per-op story, largely closed).

**Target.** Corpus total `tools/corpus/workloads` at **≤ 2× node** before v1.0.
Today: **2171.2 ms vs node 743.8 (2.92×)**, measured this session (81 rows,
min-of-5, order reversed each round, differential exact on all 81; §1.1).

**Definition of done.**
- Every corpus row that has a loop and a call lifts into the tier (§3), and its
  lifted IR has **no `FrameLoad`/`FrameStore` per iteration that `mem2reg` could
  have promoted**, and no `Value` round trip inside a typed chain.
- A live heap reference held in a register is traced at every safepoint
  (`--gc-stress` and `--gc-verify` exact, no barrier/UAF class bug).
- An inlined callee's premises are retired (not deopted) on invalidation, and
  the interpreter resumes at a step boundary with the operand stack exactly as
  the interpreter would have it.
- Every row's result identical in jit / jitless / `--gc-stress` /
  `--nursery-stress`; test262 `language`, `built-ins`, `annexB` at 0 failures
  in both modes; `clippy --workspace --all-targets -D warnings`;
  `cargo test --workspace`.
- The remaining distance to a V8/SM-class JIT is written down, with a number
  attached to each part of it.

---

## 1. Where we are, measured

### 1.1 The corpus, same session

`target/release/slag.exe --corpus tools/corpus/workloads` (and `--jitless`),
five rounds with the column order reversed each round, min per row,
`node tools/corpus/run_node.js` as the denominator (all four columns share the
`bench_once` protocol):

| column | total | vs node |
|---|---:|---:|
| slag jit | 2171.2 ms | **2.92×** |
| slag jitless | 4622.7 ms | 6.21× |
| node jit | 743.8 ms | 1.00× |

The tier is 2.13× the interpreter (4622.7 / 2171.2). Every row's completion
value agrees across slag-jit, slag-jitless and node-jit.

Two caveats on the totals, both from the protocol (§6). First, the corpus is now
81 rows, not the 77 the morning revision counted. Second, and worse: four
passes of the same protocol in this session gave `2775.6 / 905.6` (3.06×),
`2171.2 / 743.8` (2.92×, the 5-round table above), `1875.8 / 699.7` (2.68×) and
`2028.0 / 792.6` (2.56×). Both engines move together, and the same-binary
`jitless/jit` ratio moved with them (2.10–2.44×), so neither an absolute total
nor a ratio is an instrument across windows — only a same-session, interleaved,
min-of-N comparison is. Quote the protocol, the round count and the ratio, and
compare like with like.

The row-level picture (unchanged in shape across both passes) is that the loss
concentrates on a handful of rows: `head-let` 358.8 vs 50.1 (7.2×),
`template-literal/evaluation-order` 247.7 vs 26.6 (9.3×), `construct_churn` 58.9
vs 11.0, `method_call` 52.4 vs 5.6 (9.4×), `recursive_fib` 47.6 vs 7.4,
`compound_assign` 41.7 vs 2.3 (18×). Rows below ~1 ms on node (`async_churn`,
0.28 ms) are measuring harness overhead, not an engine gap; do not read their
ratios.

### 1.2 What has landed since the 2026-10-09 snapshot

The morning revision listed these as future work. They are in the tree:

| piece | commit / file | note |
|---|---|---|
| fused canonical `for` lift (S1+S2) | `74f7c111`, `opt/lift/mod.rs` | `BuilderBind/Store{None}`, `FastLoopBind/Store`, `FastLoopHead`, the Acc steps, `RunRegBody`'s numeric leaves |
| `CallIntrinsic` lift | `c1f1c53b`, `ir.rs::Op::Intrinsic` | the lowering keeps the per-step fast path and falls back to the general call |
| argument-vector + declare + context steps | `opt/lift/mod.rs` | `ArgsBase/ArgsPush/ArgsSpread`, `FunctionDecl`, `ContextLoad/Store/Init`, `NewClosure` |
| literal creates (I6-0) | `opt/lift/mod.rs` | `ArrayFast`/`ObjectFast`/`ObjectBegin`/`ObjectInitName`/`Array*` |
| typed `Number` lane | `44b37370`, `pass/unbox.rs` | raw f64 in registers, `Op::Canon` at every non-arithmetic consumer |
| trial inlining (D1–D4) | `0be87f2f`, `89c89c4a`, `276979d4`, `pass/inline.rs` | `Op::GuardCallee`, recursive splice, guard-free-callee gate, callee slot promotion, mismatch falls back to the call |
| literal scalar replacement (E, narrow) | `70fa450b`, `9189ec80`, `pass/scalar.rs` | whole `ObjectFast`/`ArrayFast` bodies whose reads are all constant-keyed |
| materialization census (B0) | `opt/census.rs`, `JIT_DUMP_IR` | per-iteration conversions by cause, printed with the IR dump; §1.3 |
| named lift refusals (A3) | `opt/lift/mod.rs` | `opt bail` names the refused family (`HoistGuard`, `Switch`, `Try`, `Generator`, …), `opt-refuse` the exact step |
| negated comparisons lift | `opt/lift/mod.rs` | `!=`/`!==` as the base comparison plus a pure `Not`, so a body whose hot callee is `x !== 0 || 1/x === 1/y` enters the tier |
| closure phase instrument | `function::phases`, `SLAG_CLOSURE_PHASES` | per-phase TSC cycles per closure, overhead-calibrated; §4 A0 |
| `Int`-op operand check | `opt_lower.rs` (`opt:int-operand`) | a wrapping `i32` op over `Value`-word operands refuses instead of `iadd`ing tag bits |
| CSE + LICM | `pass/cse.rs`, `pass/licm.rs` | in the pipeline |
| coarse typer (F3, partial) | `pass/guard.rs`, `Op::GuardType` | `Number` only, retire-don't-deopt |

### 1.3 The B0 census: what the rest of Phase B is worth

`crates/jit/src/opt/census.rs` (new) counts, per body and per iteration, every
place a value leaves a register representation — `Type::Int` (an i32 register)
or `Type::Float` (a raw f64) — for a `Value` word, and charges each one to §2's
materialization points. It prints after the `JIT_DUMP_IR` dump, so the number
and the IR that produced it are in one place.

Definition (the exact/vs/heuristic split matters): an **`Op::Canon`** is a raw
f64 leaving the chain; its cause is the op that consumes it (`unbox` inserts it
directly in front, so the consumer is the reason). An **`Int` operand of a
non-int-native op** is an i32 leaving; the int-native set is
`IntAdd/IntSub/IntMul/BitAnd/BitOr/BitXor/Shl/Shr/UShr`. A conversion's cause is
`frame` (a slot access), `guard` (a deopt mirror), `alloc` (a literal build),
`call` (a call step, or an op whose helper *is* the operation, e.g. a member
read), `compute` (a plain arithmetic/comparison/conversion op — **the Phase B
budget**), or `other`. The counts are exact for the two mechanisms (`Canon`
sites, `Int` widens); the five-way split is a heuristic documented in the module.

One scope caveat: "per iteration" means "the cycle-resident instructions", so a
body with several cycles (`loops N` in the report) counts each cycle's traffic,
including cycles that are **mutually exclusive at run time** — a hoist guard's
guarded and general copies, for instance. Read the number as an upper bound for
such a body, and compare it with `loops` before quoting it.

Measured over the 63 rows that lift (18 do not; §3), worst body per row, **per
iteration**:

| statistic | median | max | sum over rows |
|---|---:|---:|---:|
| all conversions | 2 | 10 | 176 |
| of which `compute` (avoidable) | 1 | 10 | 117 |
| frame stores | 1 | 6 | 101 |
| frame loads | 1 | 8 | 123 |
| `Canon` (raw f64 → word) | 0 | 3 | 29 |
| `Int` → word | 1 | 7 | 147 |

Only six rows have ≥5 avoidable conversions per iteration
(`opcost/array_for_each_js` 10, `math_intrinsics` 5, `element_read` 5,
`element_write` 5, `char_ops` 5, `coercion_concat` 5). 16 rows have any
`call`-caused conversion, 14 any `frame`-caused, 3 any `guard`-caused.

The probe is reproducible: `python tools/corpus/census_probe.py` (per-row
table), `python tools/corpus/refusal_probe.py` (§3's bail reasons), and
`python tools/corpus/measure.py --rounds 5` (§1.1's interleaved timing). All
three take `--slag PATH`, default to `target/release/slag{,exe}`, and live in
`tools/corpus/` beside the runners they drive.

**This is the decision the morning revision said must come before Phase B, and
it comes out against the phase as scoped.** That revision expected "20–40
avoidable round trips per body at 2–4 ns… 40–160 ns of a 428 ns body". The
measured budget is **1–2 conversions per iteration on the median row** — 2–8 ns
— and 10 at most. At `method_call`'s 32 ns/iteration, its one avoidable
conversion is ~10% of the row; on most rows it is below the noise floor of the
measurement protocol. B cannot be the lever for a 3.06× gap. What the census
does say is that the two mechanisms that *produced* the level-3 lanes
(`canon` median 0, `int` median 1) are working as designed: the values that are
in registers stay in registers, and what remains is the residue the four
materialization points legitimately require.

---

### 1.4 Where the two rows that dominate the gap actually spend it

The two language fixtures are ~45% of the corpus gap (head-let 335 ms, template
227 ms on a 2026-11-14 pass). Ablating head-let (100k bodies per call, ms):

| variant | ms | delta |
|---|---:|---|
| full fixture | 327 | |
| `for-in` + member stores, no closures, no asserts | 69 | enumeration + stores = 69 |
| closures per iteration + `for-in`, no asserts | 293 | **+224 for the closure path** |
| 3 closures + 3 calls, no `for-in` | 153 | closure path alone |
| template fixture, no asserts | 116 | asserts = 140 of its 256 |

Microbenchmarks (200k iterations, opt tier) price the pieces: a closure created
and called **581 ns/iteration** (no capture 487, created-but-never-called **339**,
arrow instead of function 571, escaping into a property 547), while a hoisted
closure called per iteration costs **7 ns**, a plain object literal is
**2.3 ns** (scalar-replaced), and node eliminates the non-escaping closure
entirely (0.6 ns/iteration). So **function-object creation is ~340 ns**, two
orders above an object literal, and the call itself is ~7 ns.

The helper census agrees on the shape (per body, from `JIT_HELPER_STATS`):
head-let makes 21 helper calls (`CertifiedCall` 3, `UnarySlow` 2, `BinarySlow` 2,
`ForInNext` 1.3, `PerIteration`/`LoadPerIter`/`StorePerIter` 3, `CreateFunction`
1, `AssignMemberName`/`AssignMemberComputed`/`FastArrayElementWrite`/`LoadIdent`/
`TypeofTop` 1 each); the template fixture makes 26 (`UnarySlow` 5,
`CertifiedCall` 5, `LoadContext` 3, `ArgsPush` 3, `TemplateSub` 2, then the
template/tag/context steps).

Two consequences for the ordering, and they are the reason §4 now starts with
A0 rather than a lift:

- **The gap in these rows is creation and call machinery, not register traffic.**
  B0 (§1.3) measured 1–2 avoidable conversions per iteration; the closure path
  is 340 ns *per closure*. Lifting `is()` (a 30-step body every assert calls)
  was worth ~2% on both fixtures by same-binary A/B — real, but not the lever.
- **The biggest row's hot body is still outside the tier**: `__t262Body` is 101
  steps and bails on `EnterPerIteration`/`PerIteration`/`LoadPerIteration`/
  `StorePerIteration`/`LeaveBlock`/`TypeofTop` — the `for (let x in obj)`
  per-iteration environment family. Lifting it is *necessary* for the row, but
  by the ablation it cannot be *sufficient*: the closures it creates cost 340 ns
  each and the tier does not change that.

---

## 2. The value model: what "out of the `Value` stack" means

A value in this engine can live at four levels. The tier's job is to move every
value as far down this list as its uses allow, and to know *exactly* when it
must climb back up.

| level | storage | what a use costs |
|---|---|---|
| 0 | a slot of `vm.stack` (the operand stack) | a store + a load per step |
| 1 | a frame slot (`vm.frame`, `ScopeInfo` slots) | a store + a load per access; **is a GC root** |
| 2 | an SSA value (`ValueId` → a Cranelift register or spill) | free, but invisible to the conservative stack scan |
| 3 | a typed SSA value (`Type::Int` = i32; `Type::Float` = raw f64) | free **and** no tag check, no `canon_double`, no range guard |

### 2.1 The four materialization points

A value at level 2/3 must be written back to its level-0/1 home before exactly
these events. Every one of them must be *enumerated by the pass*, not assumed:

| event | who can observe | today |
|---|---|---|
| a helper that reads the frame (`Vm::frame_get`: `builder_bind`/`builder_store`, `create_function_decl`, `create_arguments`) | the callee's frame slots | `mem2reg`'s `slots_unobserved` refuses promotion for the ops that name a slot implicitly (`FunctionDecl`, `ForOfBindLocal`, `ForOfNextBindLocal`) |
| a guard's deopt branch | the resumed interpreter reads the operand stack **and** the frame | operand stack mirrored (`emit_guard`); frame via `mem2reg::insert_flushes` |
| a GC safepoint (any allocation, any helper that can allocate) | the collector's stack scan of the frame; the operand stack | **refuse**: `non_heap_slots` promotes only slots whose every store is a non-heap immediate, so a heap value keeps its rooted frame slot (§4, C) |
| a call (the callee may observe its caller's frame through `arguments`/`eval`) | the callee's frame | not tracked per value; `slots_unobserved` documents why a call cannot reach the frame (`CreateArguments` never lifts, `eval` never lifts) |

The invariant from `optimizing-tier-plan.md` §2 still holds and is what keeps
this affordable: **the compiled frame never diverges from the interpreter
frame.** Level 2 is a *caching* layer over level 0/1, never a replacement — so a
flush is always a store to a slot the interpreter would have written anyway, and
no snapshot/`Recover_*` machinery is owed.

The safepoint row is the one that is still missing infrastructure. A level-2
heap reference must be traceable. Two options, in cost order:

- **(a) Refuse.** Keep a heap-typed value at level 1 (its frame slot is a root)
  whenever it is live across a safepoint. This is what the tree does today and
  it costs the materialization, not correctness.
- **(b) A stack map.** Cranelift emits user stack maps; the collector walks them
  for the code range instead of scanning the frame conservatively. There is no
  `stack_map` anywhere in `crates/` today.

### 2.2 What the tree actually implements

Level 3 is **two** representations, not three. `opt_lower.rs::repr` maps
`Type::Int → I32` and `Type::Float → F64`; everything else, including
`Type::Bool`, is an i64 `Value` word. So:

- The `Int` lane exists, with `int_to_bits` as its one conversion at a
  `Value`-word boundary.
- The `Float` lane exists, with `Op::Canon` as its one conversion.
- **`Type::Bool` in the lattice is a word type, not a register representation.**
  `Lt → Branch` still materializes a tagged boolean through
  `canonical_bool`/`raw_code_to_bool`. The morning revision's B1 claim that a
  Bool lane "subsumes the `Lt → Branch` fusion" describes work that did not
  happen; adding one is its own cut (§4, B3), and the census will show whether
  it is worth anything (it charges those conversions to `compute`).
- `pass::normalize_param_types` joins block-parameter types so an edge and its
  target stay in one register class; `Type::Float` joins with `Int`/`Number` as
  `Float`, because a canonical number word is a valid raw f64.

The leaks the morning revision listed are now mostly closed:

- **Every helper boundary re-enters level 0/1.** Still true, and measured
  cheap: the helper funnel is ~10 ns and §1.3 says the conversions it forces are
  ~1 per iteration.
- **`canon_double` per arithmetic value.** Closed for chains: `unbox` retypes a
  proven-numeric chain `Float` and inserts `Op::Canon` only at the boundary
  (`canon_double` median 0 per iteration, max 3).
- **A call, a guard, a frame-observing helper, or a GC safepoint forces a
  materialization.** `mem2reg::insert_flushes` handles the guard; the safepoint
  is handled by refusal (§2.1).

---

## 3. Coverage: what enters the tier and what does not

Measured with the same probe (`JIT_DUMP_IR`): **63 of 81 rows** produce a
census, i.e. their body compiles into the optimizing tier and lifts. The 18 that
do not, with the bail reason the lift reports and the distinct blocker classes
from `opt-blockers`:

| reason (`opt bail`) | rows | blockers the probe names | what it is |
|---|---|---|---|
| `Step("HoistGuard")` | 6 (`declarative_read`, `nested_read`, `object_read`, `own_read`, `proto_read`, `obj_prop`) | — (a terminator refusal) | the LICM hoist guard, refused **by design** (an always-miss opt body is a measured pessimization; `emit_fused_test`) |
| `Step("RunRegBody")` | 6 (`json_roundtrip`, `many_objects_read`, `array_slice`, `array_to_sorted`, `prim_prop`, `split_join`) | — (a leaf-table refusal) | a `LeafOp` outside the lifted subset: `GetMemberNameLocal`/`GetMemberComputedLocal`, `BinStoreReg`/`StoreReg`, `PushAcc`, sometimes `CallFast` |
| `Step("Switch")` | 1 (`switch_dispatch`) | `Break`, `SwitchDisc`, `SwitchTest` | the switch step family has no lift |
| `Step("Try")` | 1 (`try_catch_loop`) | `CatchBind`, `EnterTry`, `Exit`, `LeaveBlock` | the try/catch step family has no lift |
| `Step("Generator")` + `Step("UpdateContextSlot")` | 1 (`generator_loop`) | `UpdateContextSlot`, `Yield` | generator suspension, and the captured-slot update |
| `Step("FusedLoop")` | 1 (`search_slice`) | — (a shape refusal) | `BuilderBind { slot: Some(0) }`: the string-builder form, not the acc-path |
| `Step("ObjectLiteral")` | 1 (`object_spread_fresh`) | `ObjectSpread` | object spread |
| `Step("RegExpLiteral")` | 1 (`regexp_test`) | `RegExpLiteral` | a regexp literal has no lift |

Two things follow:

- **The largest bucket is a refusal, not a gap.** Six rows (`globals/*_read`,
  `objects/own_read`, `objects/proto_read`, `opcost/obj_prop`) are blocked on
  `HoistMemberGuard`/`HoistGlobalGuard`. The lift deliberately refuses them: it
  models the guard as "always miss", and that costs `objects/own_read` +571% and
  the `globals/*` rows ~+60% each against `SLAG_OPT=0` (measured, min-of-3). The
  per-step tier takes the hit and reads once. Modelling the probe instead was
  attempted (A2, §4) and is **blocked on the type prover**, not on the probe.
- **The `Step("RunRegBody")` bucket is the S3/S4 gap.** `optimizing-tier-impl.md`
  records that the register-body lift covers S1+S2 (the scaffold plus the
  arithmetic body) and not S3 (member reads) or S4 (stores). The refusal now
  prints the exact leaf table, and it is the same ops the `objects/*_read` rows
  need: `GetMemberNameLocal`, `GetMemberComputedLocal`, `StoreReg`,
  `BinStoreReg`, `PushAcc` (and `CallFast`, in the two rows that call per
  iteration).

The anonymous bucket is gone (A3, §4): every refused body now names a family,
and the precise variant comes from the `opt-blockers` / `opt-refuse` lines the
probe prints beside it.

---

## 4. The remaining work

Ordered by measured impact, then by dependency (§1.3 and §1.4 are the evidence).
**A0** is the corpus's largest single term; **A** is coverage (no perf claim of
its own); **C** makes a register-held heap value sound; **D5** is the one call
lane left; **F1** is the oracle the read/arith work needs. The B0 probe (§1.3)
retires most of Phases B and E *as scoped*.

**A0 — function-object creation (~340 ns each).** The measured dominant term in
the corpus's two largest rows (§1.4): three closures per body cost ~1 µs of
head-let's 3.3 µs, against node's 570 ns for the *whole* body.

The phase instrument (`SLAG_CLOSURE_PHASES=1`, `function::phases`) splits the
path into TSC cycles per closure, with an empty timed block measuring the
instrument's own per-phase cost (~31 cycles, subtracted below):

| phase | cycles/closure | what it is |
|---|---:|---|
| `source_info` | 230 | the `SourceInfo` cache lookup + slice/hash for `Function.prototype.toString` |
| `props` | 199 | `length`/`name` (+ sloppy `caller`/`arguments`) adoption |
| `register` | 195 | the `EcmaFunction` record insert into the agent's table |
| `function_new` | 169 | the `Function` object allocation with its prototype |
| `record`, `strict`, `body`, `params`, `compile`, `patterns`, `realm`, `context` | ~60 total | every cached lookup is cheap; the record build is ~25 |
| **unaccounted** | **~446** | outside the instrumented phases: the `outer_chain`/`per_iteration_chain`/`named` clones passed by value at each creation site, and the call plumbing |

The picture is diffuse — four ~200-cycle items and a third of the cost in
plumbing — so there is no single 2× cut:

- **A0a — `source_info` laziness** (230 cycles): resolve the source slice only
  when a consumer asks (`toString`, a span-carrying frame). Nothing else needs
  it at creation time.
- **A0b — the caller-side plumbing** (~446 cycles, the largest chunk): drop the
  by-value environment/chain/name clones at the creation sites and instrument
  the caller side to confirm the split before moving code.
- **A0c — `props` and `register`** (199 + 195): defer `name`/`length` behind the
  same pending flag the `prototype` already uses, and fold the record into the
  function box (or at least avoid a second allocation for it).

**Accept:** the closure micro (§1.4) falls; both fixtures fall with it; the
corpus differential and `--gc-stress` stay exact (a closure is a heap object with
an environment edge — the rooting traps in §7 apply).

**A1 — the corpus's biggest body cannot enter the tier.** `__t262Body` in the
`head-let` fixture is 101 steps and bails on the `for (let x in obj)`
per-iteration environment family (`EnterPerIteration`, `PerIteration`,
`LoadPerIteration`, `StorePerIteration`, `LeaveBlock`) plus `TypeofTop`. Each is
a helper call in the per-step tier (`enter_per_iteration`, `per_iteration`,
`load_per_iter`, `store_per_iter`, `leave_block`, `typeof_top` are all in the
helper table), so this is a mechanical lift of six op families, and it is a
prerequisite for A0 paying anything on that row: the tier cannot make the body
faster while the body is not in the tier. Expect coverage, not speed — the
`is()` body (§1.4) lifted at ~2%. **Accept:** the body lifts; the row ≤ its
per-step time; helper census not worse.

**A1b — the register body's member leaf ops (S3) and stores (S4).** Lift
`GetMemberNameLocal`/`GetMemberComputedLocal` to `MemberLoad`/`MemberCellLoad` +
`MemberGuard` (read-only first, as S3 was scoped), then `BinAccPop`, `UpdateAcc`
and the member stores. Probe: `JIT_DUMP_STEPS` bodies per row, before/after, and
for each newly lifted body its helper census against the per-step body's.
**Accept:** every newly lifted row ≤ its per-step time; corpus differential
exact; the `installed_jit_*_inlines` tests pass without forcing.
**Stop rule:** a newly lifted row >2% slower means a missing parity port — port
it before proceeding, never carry a regression into the next cut.

**A2 — the hoist probe: blocked on the type prover (attempted and measured
2026-11-14).** The largest coverage bucket (§3, six rows) is not a missing step
but a deliberately refused terminator: `emit_fused_test` models
`HoistMemberGuard`/`HoistGlobalGuard` as "always miss", which measured
`objects/own_read` **+571%** and the `globals/*` rows ~**+60%** each against
`SLAG_OPT=0`, so it refuses instead.

The probe itself is easy to model and was implemented and measured twice. Attempt one: an `Op::HoistGuard` lowered to one helper call (`hoist_guard`, running the interpreter's own probe through a shared `Vm::hoist_member_guard` / `Vm::hoist_global_guard`), branching to the guarded copy on a hit and the general copy on a miss. Every one of the six rows lifted, and every one got **slower**: opt / per-step, min-of-5, same binary, `own_read` 3.52, `nested_read` 1.54, `object_read` 1.44, `declarative_read` 1.44, `proto_read` 1.08, `obj_prop` 1.00 (the last also returned a wrong value). Attempt two added slot-precise promotion — the probe's slots carried in `Imm::Hoist` and flushed before it, so the loop accumulator stays an SSA candidate instead of forbidding promotion of the whole frame: 1.36–3.62× still, `obj_prop` at parity only because its int path now refuses. Both attempts are reverted.

The blocker is the body's shape, not the probe, and it is now precise:

- A guarded body contains **two copies of the loop** (guarded and general)
  writing the same frame slots, and `narrow`'s slot prover is whole-body and
  flow-insensitive. The general copy's imprecise store is evidence against the
  guarded copy, so the guarded copy's `+` lowers to the branchy checked path
  (or worse, `binary_slow`) while the per-step tier gets a bare f64 add — the
  guarded copy measured 0.72 ns/iteration per-step against 2.55 ns in the opt
  tier. The guard's own facts (`prim_slots`, `numbers`, `int_slots`, `reads`
  with `strict`) are exactly the missing proof.
- The lift discards the one proof it is handed: a numeric register-body leaf
  (`BinStoreNum`, which the compiler emits only from a numeric plan) is lifted
  through `binary_type(Op::Add)`, which is `Unknown`, so the guarded copy starts
  unproven even before the general copy poisons the slot.
- The two-copy shape also exposed a latent contract violation: `IntAdd` over
  operands `narrow` had demoted to `Value` words lowered to `iadd` on tag bits
  (`obj_prop` returned 4.6e18 instead of 700000). The lowering now refuses
  (`opt:int-operand`, tested), so the class is a per-step fallback rather than a
  miscompile.

So A2 is two cuts, in this order: **(a)** make the type facts region-scoped — the
guard's payload applied to the blocks its hit edge dominates (a forward
must-analysis: intersect the facts over each block's predecessors, with the
hit edge adding the guard's own), which also fixes the lift discarding
`BinStoreNum`'s proof — then **(b)** re-land the probe model: an
`Op::HoistGuard` (`Imm` = the step index and the slots it addresses,
`Type::Bool`, `Effects::call()`) lowered to one `hoist_guard(ctx, step)` helper
call that returns the hit and has already written the hidden slots, the lift
arm returning `(hit, false)` so a hit falls through to the guarded copy and a
miss branches to `jump_target`, and `mem2reg` flushing a promoted slot before
the probe that names it. **Accept:** every one of the six rows ≤ its per-step
time; `HoistGuard` drops out of the refusal census.

**A3 — name the emit refusal (landed 2026-11-14).** `step_name` now names every
family that has appeared as a corpus first blocker (`HoistGuard`, `Switch`,
`Try`, `Generator`, `UpdateContextSlot`), and the second-pass terminator refusal
(`emit_fused_test`) reports through `report_refusal` like the per-step path, so
`opt-refuse:` names the exact step and `opt bail` names its family. The
anonymous `Step("step")` bucket is gone from the corpus; the probe's own
history is the reason it existed (`report_refusal` had one call site, and
`step_name`'s catch-all was the only name for a try/switch/generator step).

**A4 — `SwitchDisc` and the register-body leaf ops come next**, in whichever
order the refusal census shows the most rows behind. Neither has a measured
pessimization attached yet, so both are parity cuts, not re-enablers.

**B (residue).** The census says the median row has 1 avoidable conversion per
iteration, so the phase as written is not worth its risk. What survives:
- **B1 — the `compute`-caused `Int` widens** (`int` median 1, max 7): each is an
  op with no int lane. Only worth doing per-op, with the census row as the
  before/after, and only on the six rows with ≥5.
- **B2 — the `Float` boundary.** Already correct (`canon_double` at the
  materialization, one funnel); keep the census's `raw f64 without Canon`
  invariant at zero. It is the cheapest check that a later pass did not break
  the funnel.
- **B3 — a `Bool` lane** (i1/i8) is the one representation change with a
  plausible payoff (`Lt → Branch` and every `JumpIfTrueKeep` chain), but the
  census charges those to `compute` at ≤2/iteration on the median row: measure
  before building it.

**C1 — keep the refusal, and make it explicit.** Today `non_heap_slots` is the
policy but nothing states it as one: a heap-typed slot is never promoted, so a
heap value lives at level 1 and is rooted. Document it at `promote_frame_slots`
and add the test that a promoted slot's *load* after a call re-reads the frame
(the slot-observing-call trap, §7).

**C2 — the stack map.** Cranelift user stack maps per safepoint; the collector
walks them for the PC's code range. Prerequisite for any cut that keeps a heap
reference at level 2 (a generalized E, an inlined callee's context) — but not
for anything above, since both refusal and the landed splice keep heap values in
frame slots. **Accept:** `--gc-stress` and `--gc-verify` exact on every row whose
body holds a heap value across an allocation; a targeted test that allocates
under a live register-held reference with `--nursery-stress`.

**D5 — the callback lane.** `array.forEach(cb)` and friends re-enter the generic
call funnel per element (52.6 ns/call vs 5.0 ns for the same callee called from
compiled code, 2026-11-13). With the splice landed, a certified callback is an
inlined loop body. **Accept:** `array_for_each_js` and `map_for_each` fall,
differential exact, `JIT_MAX_COMPILE_STEPS` body sizes recorded.

**E (residue) — escape analysis beyond literals.** `pass/scalar.rs` already
scalar-replaces a whole `ObjectFast`/`ArrayFast` whose reads are all
constant-keyed. Generalizing it needs a body big enough to contain both the
allocator and the consumer (i.e. D5-scale inlining) and C2 for a field that
holds a heap value. Probe: `--gc-trace` allocation count per row. **Stop rule:**
if the surviving allocation count barely moves, go back to A, not deeper into E.
Do not re-open the `JsObject` layout for allocation speed: shrinking it is a
measured non-win (`{}` −1.4%, `[]` +32%, corpus +2…5%).

**F1 — `clobberize`.** Effects are over-approximated (`default_effects` gives
bit ops and comparisons `Effects::call()`). This is now the *next* thing with a
measured consumer, unlike on 2026-10-09: `mem2reg`'s slot promotion, `licm` and
`cse` all run, and the census reports the conversions an over-approximation
forces. Order it after A1/A2 (coverage) and D5 (body size), and measure the
census before and after the narrowing.

**F2 — GVN/load elimination.** LICM and CSE have landed; the remaining read rows
(`proto_read`, `many_objects_read`, `compound_assign`, `element_*`) are
coverage-blocked (§3), not pass-blocked, which is why they come after A1.

---

## 5. Ordering

```
A0 (function-object creation)  <- A1 (the 101-step body's env family)  <- A1b (S3/S4)
        |
        v
   D5 (callback lane: inlined loop bodies)   F1 (clobberize) -> F2 (GVN/loads)
        |                                          ^
        v                                          |
   E (generalized EA)  -- needs C2 (stack map) -----+
```

A3 (naming the refusal) is landed; it is what makes this ordering readable, and
it is the first cut of any future coverage arc, because a bucket you cannot
name is a bucket you cannot size.

- **A0 gates the corpus total**: the two fixtures are ~45% of the gap and the
  closure path is ~2/3 of the larger one. Coverage (A1/A1b) is a precondition
  for A0 on those rows, but the allocation cost is what has to move.
- **D5 before E**: escape analysis has nothing to work on until a call boundary
  disappears, and the callback row is where the call traffic is.
- **C2 before any level-2 heap value.** Nothing above requires it, because the
  tree's policy is refusal; the first cut that keeps a heap reference in a
  register pays for the stack map.
- **F1 after A and D5**, because narrowing the effects oracle only pays once the
  reads and calls it governs are inside a lifted body.
- **B is no longer on the critical path.** The census (§1.3) is the number the
  morning revision said would decide it, and it says the residue is small.

---

## 6. Measurement protocol

Non-negotiable, from the accumulated lessons (`.agents/skills/slag-perf-measurement`):

- **One row per process**, isolated directory, **interleaved A/B**, min-of-N.
  The whole-corpus total moves ±0.7% run-to-run on a *fixed* binary and its
  outliers more; a binary-vs-binary diff of the corpus total is load-sensitive.
  Measure it with the order alternating between rounds and report the matched
  pairs (the §1.1 table is that protocol).
- **A lift change is judged on the helper census first** (`JIT_HELPER_STATS`,
  one row per process): a lifted body must not call more helpers than the
  per-step body it replaces.
- **A lift change is judged on the materialization census second**
  (`JIT_DUMP_IR`): the body's conversions per iteration must not rise, and the
  `raw f64 without Canon` count must stay zero.
- `SLAG_OPT=0` is the per-step control in the same binary.
- `JIT_DUMP_IR` is the evidence that a body was promoted/typed/lifted; a timing
  without the dump is a guess.
- Never compare `iter` and `release` numbers. Kill stray processes; the host's
  own load has produced a false "+51%" and a false "−3.0%" in one session.
- Gates for every cut: corpus differential 0 in jit/jitless/`--gc-stress`/
  `--nursery-stress`; test262 `language` and `built-ins` at 0 fails both modes
  (`--timeout 15`); `cargo test --workspace`; clippy `-D warnings`;
  `cargo fmt --check`.

---

## 7. Traps (consolidated)

Still live from the earlier notes: **`JIT_MAX_COMPILE_STEPS`** (128/1024 —
inlining grows bodies and an oversized one silently returns to the interpreter);
**GC roots in optimized frames** (level 2 is invisible to the conservative scan,
and a promoted *heap* slot unroots it — the barrier-UAF class); **resume
fidelity** (`e.stack`, `using` disposal, inlined-frame naming have each broken
once; retirement makes resumption the common path); **the leaf lane** (no deopt
path); **the helper ABI is a four-file mirror**; **sealed-block discipline**;
**single-sourced ops** (the lift, passes and lowering all target `opt::Op`);
**the interpreter is the oracle** (faster, never different); **a guard is
`pure()` by effects but deopts**; **`Op::Eq` is loose `==`**; **a slot-observing
call can *write* the slot** (reload after a barrier, never reuse); **the entry
block cannot carry a phi**.

Kept from the morning revision, still true:

- **A typed value must be widened at every mirror.** A raw i32 stored into the
  operand-stack mirror corrupted the slot the interpreter resumes from
  (`opt_tier_int_value_survives_a_guard_deopt`). With four materialization points
  and two register representations, the widening belongs in exactly one
  function per representation (`int_to_bits`, `Op::Canon`) or the matrix is
  unmaintainable.
- **`canon_double` is load-bearing at the `Value` boundary, not at production.**
  Sinking it is sound only if *every* producer of a raw word goes through the one
  conversion; a missed one is a NaN reading as a tag, i.e. a crash, not a slow
  path. The census's `raw f64 without Canon` counter exists to make a miss
  visible without a crash.
- **Megamorphic call sites make per-id caches a loss.** A `certified_cache` in
  the `leaf_cache` shape measured `head-let` **+4.1%** (2026-11-14): its
  `fns.a()` site is a fresh closure every iteration.
- **Intrinsic/entry-point lanes are nearly exhausted** (~10 ns each). Do not
  propose another one as a perf item without a per-row measurement showing the
  funnel.
- **A lift change that enters the tier for a shape the per-step path inlines
  better is a regression**, and the test suite is the only thing that catches it
  (`installed_jit_*_inlines` count helper calls). Port the fast path first.

Added 2026-11-14 by the B0 census:

- **`Type::Bool` is a lattice type, not a register representation.** `repr`
  gives it I64; do not write a pass that assumes an i1 exists, and do not budget
  a `Lt → Branch` win until a Bool lane is built.
- **`opt bail` and `opt-blockers` answer different questions.** `opt bail` is
  the coarse family (`step_name`), `opt-blockers` the precise step variants that
  fail `stack_delta` when `JIT_DUMP_STEPS` is on, and `opt-refuse` the single
  step an emit actually failed on. A shape refusal (a `LeafOp` outside the
  register-body table, the string-builder `BuilderBind { slot: Some(..) }`) has
  no blockers at all. Read all three before calling a bucket a coverage gap —
  the first version of this session's census called nine rows "shape refusals"
  when three of them were plainly step-level (`Yield`, `SwitchDisc`, `EnterTry`)
  and only the coarse name hid it.
- **A guarded body has two loop copies, and the slot prover is flow-insensitive.**
  `narrow` proves a slot numeric/int from every store in the *whole* body, so a
  hoist guard's general copy (imprecise stores) poisons the guarded copy's
  proofs. Measured twice: 1.44–3.52× the per-step tier with promotion refused,
  and 1.36–3.62× with slot-precise promotion (the probe's own operand slots
  flushed before it). Any future two-copy shape (an inlined callee plus
  fallback, for example) will hit the same wall; the fix is region-scoped facts,
  not a lifted probe.
- **A numeric register-body leaf's proof is discarded by the lift.**
  `BinStoreNum` exists only where the compiler proved the plan numeric, but
  `emit_leaf_op` emits through `binary_type(Op::Add)`, which is `Unknown`, so
  the IR starts unproven and depends on `narrow` re-deriving a fact it was
  handed. Typing the numeric leaf's result `Number` is sound and independent of
  the hoist work — a separate, cheap cut.
- **`IntAdd`/`IntSub`/`IntMul` require `Int` operands, and the lowering now
  checks.** A pass that demotes a producer to a `Value` word leaves the `Int` op
  adjacent to tag bits; `iadd` on those is a wrong value, not a slow path
  (`obj_prop` answered 4.6e18 for 700000 before the check). Treat a new `Int`-op
  producer as owing the operand-typing proof, not just the op.
- **A refusal is a parity decision, not always a gap.** The hoist-guard bucket
  exists because the lift's alternative (`always miss`) measured +571%. Before
  lifting a refused step, check `emit_fused_test`'s comment (or the per-step
  body) for the reason it was refused; the cheapest-looking coverage cut can be
  the one that already lost an A/B.
- **The census's cause split is a heuristic; its two mechanism counts are not.**
  `Op::Canon` sites and `Int` operand widens are structural; the
  frame/guard/alloc/call/compute charge is an op classification. Quote the
  structural numbers as facts and the split as evidence.
- **A parity-neutral lift is not a win, and a win needs a denominator.** Lifting
  the 30-step `is()` body (every assert calls it) measured 0.98–1.00× against
  `SLAG_OPT=0` on both fixtures: the tier was neither better nor worse, because
  the row's time is in the closures the body creates (340 ns each), not in the
  body's execution. Before crediting a lift, price the row's *creation* work
  with an ablation (§1.4); a body can be perfectly compiled and still be 2% of
  the row.
- **A refusal that moves later can expose a genuine underflow.** Widening the
  lift moved the template fixture's 40-step body from `Step("Binary")` to
  `Malformed("stack underflow")` — a latent lift bug previously masked by the
  earlier bail. Both are refusals (the body still runs per-step), but the second
  means the lift's stack model is wrong somewhere in that body's shapes; treat a
  reason change after a widening as a new work item, not as "still refused".
- **An instrument's own cost is a phase.** Two `_rdtsc` reads plus an atomic add
  measured 31 cycles, i.e. ~20% of a 160-cycle phase, and 12 phases put ~370
  cycles inside the `total` they inflate. Time an empty block beside the real
  phases and subtract it, or the split will read as nine equal slices.
- **Phases that sum to three quarters mean the missing quarter is plumbing.**
  The closure instrument accounted for ~74% of the path; the rest was the
  by-value `Rc` chains cloned at each creation site, which no phase boundary
  covered. When the parts do not add up, instrument the *caller* next — the
  cost is often not in the function being profiled.
- **A corpus total is only comparable within a session and a corpus size.** The
  morning revision's 2.70× and this session's 2.68–3.06× are not a regression:
  81 rows vs 77, and a different load window. Four passes on one binary in one
  session gave 3.06×, 2.92×, 2.68× and 2.56×, and even `jitless/jit` moved
  (2.10–2.44×). Compare like with like: same binary, same session, interleaved,
  min-of-N; treat a single-engine swing as the beginning of a measurement bug.

---

## 8. Open decisions

1. **Where the typed lattice lives.** Settled in code: `opt/ir.rs::Type` carries
   `Int`, `Number` (a canonical word) and `Float` (raw f64), with `join`
   implemented. A `Bool` representation would be the same place.
2. **Stack map vs refusal** (§2). C1 first regardless; C2's timing depends on
   whether a level-2 heap value is attempted in this arc.
3. **Whether `Op::Call` keeps a general lowering.** Yes — the splice falls back
   to it on a `CalleeEq` mismatch (`276979d4`), so the non-inlined lowering must
   stay identical in meaning (single-sourced).
4. **The inlining budget.** I5a says 128 steps captures 95/97 sites and the
   largest is ≤64. D5 should reuse that budget and measure body sizes.
5. **Tier count.** One. OSR entry and off-thread compilation stay deferred until
   this tier is at its ceiling.

---

## 9. What this plan does not do

No custom instruction selection, no custom register allocator (Cranelift's
`regalloc2` is the allocator; "the register tier" means the IR presents values as
SSA values so that allocator keeps them), no sea-of-nodes IR, no engine-global
type inference, no off-thread compilation, no OSR entry, no backend swap. The
backend is ~2–3× of any row; the front end is 90–95%, which is why every phase
above is front-end work.
