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
nor a ratio is an instrument across windows, only a same-session, interleaved,
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
| per-iteration environment lifts | `opt/lift/mod.rs`, `ir.rs`, `opt_lower.rs` | `EnterPerIteration`/`PerIteration`/`LoadPerIteration`/`StorePerIteration`, `LeaveBlock`, `TypeofTop` as helper calls; the 101-step `__t262Body` (the corpus's largest row) now compiles |
| `TemplateSub` delta fix | `opt/lift/mod.rs` | the substitution fence is a pop *and* a push; charging it −1 drifted the block depth and refused any body with a template before a conditional |
| closure phase instrument | `function::phases`, `SLAG_CLOSURE_PHASES` | per-phase TSC cycles per closure, overhead-calibrated; §4 A0 |
| `Int`-op operand check | `opt_lower.rs` (`opt:int-operand`) | a wrapping `i32` op over `Value`-word operands refuses instead of `iadd`ing tag bits |
| counter entry re-typing | `pass/loop_counter.rs` | the pass proves a counter whose increment the register lane already emitted as `IntAdd`/`IntSub`, and re-types its entry constant, so the header parameter joins `Int` instead of `Number` and the whole body stops bailing at `opt:int-operand` |
| register-body named read | `opt/lift/mod.rs` (`LeafOp::GetMemberName`) | an `x.name` read whose object is already in the accumulator (a chained read like `a[i].n`) now lifts to the same member-value cell probe the `Step` form uses; each refused leaf op is named through `leaf_op_name` instead of the opaque `RunRegBody` |
| CSE + LICM | `pass/cse.rs`, `pass/licm.rs` | in the pipeline |
| coarse typer (F3, partial) | `pass/guard.rs`, `Op::GuardType` | `Number` only, retire-don't-deopt |
| inlined prototype-chain read | `compiler.rs` (`emit_chain_cell_probe`), `cells.rs` | a named read whose receiver has a warm identity-keyed chain cell (a one-link prototype data property) is served with no helper call; every lane routes through `emit_member_read_chained`, including both register-body `LeafOp` forms |
| inline truthiness in the optimizing tier | `opt_lower.rs` (`emit_falsy`) | the per-step tier has had `emit_truthiness` since its first cut; the optimizing tier had no port, so every `if (x)` on a condition the typer did not prove `Bool` called `ToBooleanSlow` (`if (set.has(k))` is the shape). `ToBooleanSlow` traffic over the corpus goes 10.7M to 0 |
| per-iteration chain on the record | `function.rs` (`register_function`) | the chain install moved inside registration, so a closure creation no longer pays a second `ecma_functions` probe (a random access into a table holding one record per live function) after the insert. It must be in the record literal, i.e. BEFORE `shared_compiled_body`: the chain is a compile-time constant per creation site (`closure_per_iteration_chain` is fixed by the syntactic loop nesting) and `compile_body` reads it into the compiler, so installing it after the shared compile compiled the site's body with an EMPTY chain — `i` lowered to `Step::LoadIdent` (an env walk per read, and not a leaf step), which cost ~650 ns a call on the per-iteration closure (`.notes/perf.md` §2026-11-17). The nested-loop test that once suggested the opposite ordering was failing on `closure_per_iteration_chain`'s depth order (`position` instead of `total - 1 - position`), not on the ordering |
| per-site closure creation template | `function.rs` (`SiteTemplate`/`SiteCell`), `ir.rs` (`Step::CreateFunction`) | the site's first closure resolves and caches the shared body/params/source, the body-directive strictness and the compiled body; every later closure from the site reads them instead of three thread-local cache probes, the directive scan and the `compiled_bodies` probe |
| deferred arrow boilerplate | `function.rs` (`instantiate_arrow`, `ensure_function_properties`) | an arrow deferred its `length`/`name` own properties like a plain function does; `ensure_function_properties` now derives AddRestrictedFunctionProperties from `[[ThisMode]]` (lexical-this never gets `caller`/`arguments`), which is what makes the deferral sound for arrows |

The counter re-typing closes a refusal that had been hiding behind a green
sweep: `loop_counter` only recognised an `Op::Add`/`Op::Sub` increment, while
the register-body lane emits `IntAdd`/`IntSub` for a counter its int plan
proved. The entry constant (often shared, as in `s = 0`) stayed a `Number`, so
`normalize_param_types` joined the header parameter to `Number`, and
`opt_lower` refused the entire body rather than `iadd` tag bits. The pass now
accepts the already-wrapping increment and re-types the entry constants too,
which is sound on its own range proof (init, limit and delta all fit i32). Six
corpus rows were falling back to the per-step tier on this, and only those six
have different IR afterwards (checked by diffing `JIT_DUMP_IR` over all 81).
Isolated, interleaved, min-of-15 in one session: `warm_store` 41.3 to 24.1 ms,
`compound_assign` 49.9 to 30.9, `opcost/array_includes` -25%, `opcost/set_has`
-9%, `calls/method_call` -4%, `builtins/async_churn` +2.6% (inside the row's
noise; its hot loop is promise machinery, not the counter).

The named-read lift closes the other half of the same story. The register body's
`LeafOp::GetMemberName` (a read whose object is already in the accumulator, so
`a[i].n` and not the fused `a.n` form) was one of the leaf ops the lift answered
with the same opaque `RunRegBody` refusal, and it blocked six of the seven rows
that still refused the tier. It now emits the identical member-value cell probe
the `Step::GetMemberName` arm uses. Every remaining refusal is named through
`leaf_op_name`, because `RunRegBody` cannot be sized: after this cut the only
register-body blocker left in the corpus is `leaf:BinRemConst`
(`calls/recursive_fib`). Isolated, interleaved, min-of-15: `opcost/prim_prop`
1.73 to 1.16 ms, `objects/many_objects_read` 23.0 to 18.7, `opcost/array_slice`
-4 to -13%. `strings/split_join`, `opcost/array_to_sorted` and
`builtins/json_roundtrip` also lift now but are dominated by allocation and JSON
work, so their deltas sit inside a 5-20% run-to-run band and are not claimed.

A fifth cut is the largest single win of the arc: `objects/proto_read` 58.0 to
30.6 ms in one binary (`SLAG_CHAIN_PROBE=0`/unset, min-of-9 interleaved —
89% slower without the probe) and `builtins/set_churn` -6.4%, with the
corpus-wide `GetMemberName` count dropping from 82.8M to 71.5M (28.0M of that
from `proto_read` alone). The interpreter has cached prototype-chain
resolutions since Cut 23 (identity-keyed) and Cut 75 (map-keyed), but the cache
lives *inside* `get_member_name`, so compiled code paid the helper call on every
prototype read. `emit_chain_cell_probe` re-validates the identity cell inline
(receiver id/generation/name, `link_count == 1`, the first link's id/generation,
then the found link's member value cell) and declines to the helper on any
mismatch, so the failure mode is a helper call, never a wrong value.

Three things this cost, all worth writing down:

- **The register-body lane is a different arm.** `s += o.m` in a canonical `for`
  compiles to `LeafOp::GetMemberNameLocal` / `LeafOp::GetMemberName`, not
  `Step::GetMemberName`; the first version inlined only the step arm and its
  probe was never reached (`proto_read` still 28M helper calls). All three lanes
  now share `emit_member_read_chained`, for the same reason the leaf-call lane
  had to be taught each new op.
- **`JitCallContext::member_chain_cells` pointed at the MAP-keyed table**, and
  the identity table was `[Option<MemberChainCell>; N]`. An `Option` wrapper has
  no guaranteed payload offset (`offset_of!` cannot reach the fields) and a
  different stride, so the identity table is now `[MemberChainCell; N]` of
  `MemberChainCell::empty()` cells (id 0, which the map ids can never match) and
  the ctx carries both tables under their own names. A probe reading the wrong
  table just never hits, which is why this was silent.
- **A cross-binary number lied in both directions here.** Two binaries apart, the
  probe measured `proto_read` -44.6% and simultaneously `opcost/array_at` +73%,
  `arrays/push_pop` +60%, `opcost/array_indexof` +34%. Same binary with
  `SLAG_CHAIN_PROBE=0` as the control, those three are -2.9%, +0.9% and -1.5%:
  the regressions were pure layout on 1-2 ms rows. `array_dense`/`typed_array`
  decline the probe before its hash (their generation churns on every element
  write, so the identity cell can never validate), which is what makes the
  decline cost nothing measurable.
- **What the remaining 71.5M are.** They are not one shape: `prim_prop` (6.4M)
  is a *primitive* receiver (a separate cache), `push_pop` (3.4M) is an Array
  whose generation bumps on every `push` (the interpreter's
  `chain_shadow_free_without_generation` escape is not modelled in the probe),
  and `set_churn`/`map_churn`/`apply_call`/`construct_churn` are per-iteration
  fresh receivers (the identity cell can never serve them; the map-keyed cell
  can, but its hit also requires proving the receiver owns no vector-only
  `name`).

A fourth cut is the first one this arc to move the dominant row. `opt_lower`'s
`Term::Branch` called `Helper::ToBooleanSlow` for every condition the typer had
not proved `Bool`, and its `Op::Not` did the same, where the per-step tier has
inlined truthiness since its first cut. The port (`emit_falsy`) drives the
corpus-wide `ToBooleanSlow` count to zero, and layout-neutral
(`-align-all-functions=5` on both sides) it is `head-let` -3.7%,
`evaluation-order` -2.1%, `builtins/map_churn` -6.2%, with everything else inside
+-2%.

**The length of the inline sequence is the whole result.** The first version also
folded the `object`/`function`/`symbol` tags into the fast path (six more
compares, to keep `if (obj)` off the helper); it measured `set_churn` +1.7% and
`opcost/object_keys` +8.7%, and the second run of the *minimal* version (exactly
the per-step sequence: doubles plus the four value tags) measured -5.1%/-7.1% on
the same rows. A helper call is ~5 instructions plus a tiny body; an inline
sequence longer than that is a wash at best. Keep this one at parity with
`compiler.rs::emit_truthiness` and do not "improve" it.

Two measurement notes from the same cut, both of which nearly cost the result:
`opcost/object_keys` measured +30.9% under the isolated min-of-9 protocol and its
IR is identical between the binaries, so the delta was native layout; under
alignment it is 0.0%. And the whole-corpus single-pass A/B showed 38 rows moving
by 6-30% in both directions, which is why no row-level number here comes from it.

A third cut takes the closure path itself. `instantiate_function_with_source`
installed the per-iteration chain by looking the fresh record up again after
registration; carrying it into `register_function` removes that probe. The
closure-phase counter (`SLAG_CLOSURE_PHASES` on a 3-captured-closures-per-iteration
loop) reads total 3347.7 to 2501.0 cycles per closure, with the `tail` phase
(87.5 cycles) going to zero, and the same closure-shaped loop measures -6.2% and
-5.7% (min-of-15, two windows, one binary pair built with
`-C llvm-args=-align-all-functions=5`). The two rows it should move most,
`head-let` and `evaluation-order`, read +0.4% and +2.1% at min-of-25 rounds: their
own spread at this shape is +-5-15%, so the closure-path counter and the
ablation loop are the evidence, not the rows. That gap is itself the finding, and
it says the biggest row is no longer bound by closure-creation instructions but by
something adjacent (allocation and collection), which is where the next census
should start.

### 1.3 The B0 census: what the rest of Phase B is worth

`crates/jit/src/opt/census.rs` (new) counts, per body and per iteration, every
place a value leaves a register representation, `Type::Int` (an i32 register)
or `Type::Float` (a raw f64), for a `Value` word, and charges each one to §2's
materialization points. It prints after the `JIT_DUMP_IR` dump, so the number
and the IR that produced it are in one place.

Definition (the exact/vs/heuristic split matters): an **`Op::Canon`** is a raw
f64 leaving the chain; its cause is the op that consumes it (`unbox` inserts it
directly in front, so the consumer is the reason). An **`Int` operand of a
non-int-native op** is an i32 leaving; the int-native set is
`IntAdd/IntSub/IntMul/BitAnd/BitOr/BitXor/Shl/Shr/UShr`. A conversion's cause is
`frame` (a slot access), `guard` (a deopt mirror), `alloc` (a literal build),
`call` (a call step, or an op whose helper *is* the operation, e.g. a member
read), `compute` (a plain arithmetic/comparison/conversion op, **the Phase B
budget**), or `other`. The counts are exact for the two mechanisms (`Canon`
sites, `Int` widens); the five-way split is a heuristic documented in the module.

One scope caveat: "per iteration" means "the cycle-resident instructions", so a
body with several cycles (`loops N` in the report) counts each cycle's traffic,
including cycles that are **mutually exclusive at run time**, a hoist guard's
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
measured budget is **1–2 conversions per iteration on the median row**, 2–8 ns,
and 10 at most. At `method_call`'s 32 ns/iteration, its one avoidable
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
  was worth ~2% on both fixtures by same-binary A/B, real, but not the lever.
- **The biggest row's hot body is still outside the tier**: `__t262Body` is 101
  steps and bails on `EnterPerIteration`/`PerIteration`/`LoadPerIteration`/
  `StorePerIteration`/`LeaveBlock`/`TypeofTop`, the `for (let x in obj)`
  per-iteration environment family. Lifting it is *necessary* for the row, but
  by the ablation it cannot be *sufficient*: the closures it creates cost 340 ns
  each and the tier does not change that.

One correction to "not register traffic", because it is row-dependent and the frame-slot term does bind on tight arithmetic/array loops — but the mechanism is NOT what it looks like, and it was measured down to the cause (2026-11-15). `arrays/index_loop` (3M iterations of `s += a[i & 199999]`) runs 14.4 ms = 4.8 ns/iteration (~17 cycles) against node's 1.1 ns (~4 cycles, i.e. one `fadd` latency). The loop counter `i` is already a block parameter (a register); only the accumulator `s` round-trips through the frame (`FrameLoad slot2; Add; FrameStore slot2`).

Promotion machinery already exists and works: `opt/pass/mem2reg.rs` promotes frame slots, the top-level pipeline runs it (`SLAG_MEM2REG=0` is the seam), it supports guards (it runs after `guard::run` so it can materialize a promoted slot on a guard's deopt path), and it restricts itself to slots that never hold a heap value because a frame slot is a GC root. What refuses `s` is that restriction: `non_heap_slots` disqualifies a slot whose store's value is `Type::Unknown`, and `Add(Unknown, Unknown)` is Unknown — correctly, since a `+` can be a string concatenation. The only sound way to type it is the typer's read guard (`Op::GuardType` on the read that feeds the arithmetic), which `narrow` already models as producing a `Number`.

And that is what is missing on this row: the body is a single `Step::RunRegBody` ("a whole certified body can be one `RunRegBody` instead of straight-line fragments around dispatched `Step`s"), so the lift lowers `LeafOp`s directly (`emit_leaf_op` -> `LeafOp::GetMemberComputedLocal`) and **never emits a read guard** — a guard's `Imm::Int` is a *step index the interpreter can resume at*, and inside a fused register program no such step exists (resuming at the `RunRegBody` step would re-run the whole body, re-firing a getter or a store). Verified: `SLAG_TYPER=1 JIT_DUMP_IR=1` on `index_loop` shows `v15:? = ElementLoad` feeding `v17:? = Add` with `guard 0` in the census, while `push_pop`'s `.length` read in a *step*-lifted body does show `v19:num = GuardType(...)`.

So the next cut was **sub-step resume points for a register body**, and it landed for the computed element read: `Imm::RegResume { step, op, counter }` names the `RunRegBody` step plus the op after the read, `run_jit_body` hands the op to `Vm::reg_resume`, and the interpreter's existing call-withdrawal resume (`acc = pop()`, `run_leaf_ops(start)`) does the rest. `s` is now a loop-carried register: `arrays/index_loop` 14.6 -> 12.2 ms (-16.5%), `opcost/element_read` -10.4%, `opcost/array_for_each_js` -4.8%.

Two things had to be added and one refused. **Restore `Vm::loop_counter` on the deopt path** — a compiled counter loop keeps `i` in a register and never writes the field, while the interpreter's `fast_loop_inc`/`fast_loop_test` test the field, so without the restore a mid-loop deopt re-ran 504 phantom iterations. **Require an empty spill stack** (a resume truncates to the post-pop length, so a live `PushAcc` would leak). And **guard only the computed element read**: extending it to `.length` reads regressed every call- and member-heavy fused body (`control/for_in` +32%, `builtins/json_roundtrip` +19%), so the narrow rule is what ships. Bodies with a `BinStoreNum` slot (`loop_num`, the same class of unrestored scratch) keep the unguarded lowering.

What that does *not* yet reach: the same guard for `GetMemberNameLocal`, and heap-possible slots in registers (which would need C2's stack map — the promoted slot here is a Number, so it does not).

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
frame.** Level 2 is a *caching* layer over level 0/1, never a replacement, so a
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
| `Step("HoistGuard")` | 6 (`declarative_read`, `nested_read`, `object_read`, `own_read`, `proto_read`, `obj_prop`) |, (a terminator refusal) | the LICM hoist guard, refused **by design** (an always-miss opt body is a measured pessimization; `emit_fused_test`) |
| `Step("RunRegBody")` | 6 (`json_roundtrip`, `many_objects_read`, `array_slice`, `array_to_sorted`, `prim_prop`, `split_join`) |, (a leaf-table refusal) | a `LeafOp` outside the lifted subset: `GetMemberNameLocal`/`GetMemberComputedLocal`, `BinStoreReg`/`StoreReg`, `PushAcc`, sometimes `CallFast` |
| `Step("Switch")` | 1 (`switch_dispatch`) | `Break`, `SwitchDisc`, `SwitchTest` | the switch step family has no lift |
| `Step("Try")` | 1 (`try_catch_loop`) | `CatchBind`, `EnterTry`, `Exit`, `LeaveBlock` | the try/catch step family has no lift |
| `Step("Generator")` + `Step("UpdateContextSlot")` | 1 (`generator_loop`) | `UpdateContextSlot`, `Yield` | generator suspension, and the captured-slot update |
| `Step("FusedLoop")` | 1 (`search_slice`) |, (a shape refusal) | `BuilderBind { slot: Some(0) }`: the string-builder form, not the acc-path |
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

**A0 profiled (2026-11-16).** Current state of the rows that dominate the gap: `head-let` 507.8 ms against node's 76.4 (**431 ms** of the corpus gap), `evaluation-order` 289.5 against 37.9 (252 ms), `completion-values` 179.1 against 100.9 (78 ms). `SLAG_CLOSURE_PHASES=1` on `head-let` (2.1 M closures) reports 2645 units per closure:

| phase | /call | | phase | /call |
|---|---:|---|---|---:|
| `function_new` | 534 | | `record` | 77 |
| `props` | 323 | | `params` | 74 |
| `register` | 224 | | `source_info` | 68 |
| `body` | 155 | | `patterns` | 53 |
| *inside `register_function`, between phases* | ~390 | | `strict` / `context` / `realm` | 43 / 40 / 37 |
| `compile` | 88 | | `overhead` | 36 |

What the profile **rules out**, so nobody re-derives it: collapsing the four per-site cache lookups (`body` + `params` + `source_info` + `strict` = 340 units, 13%) into one entry saves at most ~3% of the closure path, because each is already a thread-local hash hit and the entry would still hash. `props` is already one shape-fork adoption (`set_function_properties_batched` → `adopt_vector_free_fields`), not four sequential defines; an anonymous closure's `name` is the shared empty string. And the record table is real but second-order: `--gc-stress`, which keeps `ecma_functions` small, moves `register` 340 → 270 and the total 2675 → 2374 (−11%).

Where the win is, in order:

- **`function_new` is two arena allocations** (`Function` box + `JsObject::ordinary_object_create`) plus the environment edge. One allocation — the function's `JsObject` inline in its box, or a dedicated closure allocation — is the single biggest item and is mechanical.
- **`register` moves a ~200-byte `EcmaFunction` into a `HashMap` per closure.** Function ids are sequential (`NEXT_FUNCTION_ID`), so a slab indexed by id with a free list removes both the hash and the rehash; that is also the gc-stress delta going away.
- **Lazy `length`/`name`/`caller`/`arguments`** (V8's shape) removes four keys from every closure's map and most of `props`. `prototype_pending` + `ensure_function_prototype` is the existing precedent for "materialize on first observation", so the pattern is established — but it is a semantic change and belongs in its own commit with a full sweep.

The first two are worth maybe 40% of the path (~70 ms on `head-let`); none of the three is reachable by trimming a phase. Benchmark: `head-let` (507.8 ms) plus `scratch/probe/closure_create.js` (per closure ~600 ns, against node's 0.58 ns — node elides the allocation entirely, so parity needs escape analysis, not this arc).

**The first two landed (2026-11-16, later). Measured −3.3% on the corpus and
−16% on `head-let`, not the ~40% the estimate guessed.** Both cuts are in; the
estimate was optimistic for the phase list it was drawn from (the instrumented
closure path fell 11%).

- **One allocation for a closure.** `Function` is now `#[repr(C)]` with its
  `JsObject` as the first field, allocated in place by `Function::allocate`, so
  the function box and its object part are one arena slot (~560 B, was 448 + 112
  in two slots). The object part is reachable as a `Handle<JsObject>` because the
  box payload *begins with* the `JsObject`: `Function::object_handle` builds it
  with `Handle::from_payload`, and the two box payload offsets are asserted
  equal at compile time in `function.rs` (a header change breaks the assert, not
  the program). `self_handle`/`function_self` are set to that handle, so
  `same_value`'s cross-kind compare now short-circuits on `ptr_eq` (same box).
  `JsObject::init_ordinary` and `JsObject::link_self_handle` became `pub(crate)`
  for the in-place writer.
- **The record table is an id-indexed slab.** `Agent::ecma_functions` is a
  `FunctionTable`: a directory of 256-record `MaybeUninit` chunks (bitmap +
  live count), freed when a chunk empties and recycled through an eight-entry
  buffer free list. Ids are still permanent (nothing reuses one), so a flat
  `Vec` would have grown with every closure; the directory is the only part that
  grows with the id space (peak id / 256 × 8 B).
- **The JIT read the old layout as a field.** `cells::chain_object_ptr` loaded
  `offset_of!(crux::Function, object)` and added `GCBOX_DATA_OFFSET`; with an
  inline object that load is garbage — the release binary segfaulted in
  `head-let` while every debug test passed. It now returns the payload address
  for both an Object and a Function (the object part *is* the payload), which
  also drops the load, the add and the `select` from every chain probe.
  `Heap::for_each_live_box`/`GcAny::is::<JsObject>` now see a function box as
  `Function`, not as two boxes (the v8 heap snapshot follows that).

Measured (interleaved same session, control binary rebuilt from the untouched
commit through `xtask build`'s cache; `SLAG_CLOSURE_PHASES` for the phase table).
The `function_new` row is the one-allocation step against its own parent; the
`register`/`props`/`total` rows are the slab step against the one-allocation
build.

| probe | before | after | delta |
|---|---:|---:|---:|
| `function_new` (cycles/closure) | 455.5 / 474.0 / 504.3 | 326.1 / 329.7 / 339.3 | **−30%** |
| `register` (cycles/closure, +slab step) | 214.5 / 222.5 / 221.6 | 130.2 / 119.0 / 122.7 | **−45%** |
| `props` (cycles/closure, +slab step) | 328.7 / 344.3 / 331.1 | 163.8 / 158.9 / 163.3 | **−51%** (mechanism unexplained; reproducible across two builds and six runs) |
| `total` (cycles/closure, +slab step) | 2501.8 / 2602.9 / 2535.0 | 2268.9 / 2213.5 / 2245.7 | **−11%** |
| `head-let` (ms, min of 5) | 526.9 | 441.1 | **−16%** (medians 533.3 → 450.8) |
| `evaluation-order` (ms, min of 4) | 270.5 | 257.4 | −4.8% (medians 278.6 → 267.0) |
| 1M-closure probe (ms, min of 5) | 520.9 | 389.8 | **−25%** (medians 542.8 → 416.7) |
| `calls/method_call` (ms, median of 8) | — | — | +1% (the slab's directory→chunk chain), `calls/*` otherwise flat |
| corpus, sum of per-row minima | 2507.4 | 2424.9 | −3.3% |
| corpus without the two closure rows | 1632.8 | 1638.6 | +0.4% (noise) |

Accept: `cargo test --workspace` (53/53 binaries), clippy `-D warnings`, corpus
differential exact in jit, jitless and `--gc-stress` (81/81 rows), `sweep all`
48,633/48,633 and `sweep intl402` 3,365/3,365. Run the sweeps with `--accurate`:
the default full-parallel run reports load-induced hangs on 5-15 s fixtures
(`TypedArray/prototype/copyWithin/coerced-values-end-detached.js` in one pass,
`Script_-_Gurmukhi.js` in another) that pass on re-run. The slab's creation win
is real (1M probe, five separated pairs) and its call-path cost is one
dependent load (measured as a wash on the corpus); the next cut is the third
item below.

**The closure-path counter is the instrument for this arc, not the rows.**
`head-let` alone spans 441-495 ms within the final binary across five
interleaved runs, so a single row reading is not evidence either way; the 1M
closure probe is `scratch/probe/closure_create.js` with the loop bound raised to
`1000000` — the 100k original is inside the noise (33-48 ms across runs of one
binary).

**The third item landed (2026-11-16, later): the boilerplate own properties are
deferred.** A plain ordinary function or arrow is born with no own properties;
`length`/`name`[/`caller`/`arguments`] materialize on the first own-property
observation. `Function::props_pending` on the closure box is the state (a
`Cell<bool>`, read by the property funnels and by the compiled chain probe),
`ensure_function_properties` is the materializer (the same pre-forked-shape
adoption the eager path used, with a structural fallback for a non-fresh
object), and the values derive from the record's `params`/strictness/kind plus
the box's `name` — which is why SetFunctionName can rename a pending function
without materializing it (it writes the box and lets the property come later).

What the change owes, and where the sweep found it:

- **Every own-property observation.** The keyed barrier
  (`maybe_materialize_deferred_own`) is the old `prototype` hook generalized to
  five atoms; the keyless form (`materialize_deferred_own_value`) covers
  own-key enumeration and integrity level. A *keyed* get/has on any other key
  needs nothing: absent boilerplate does not change another key's resolution
  or its chain walk.
- **Every structural operation, whatever the key.** A create/define/set that
  adds an own property must materialize first, or the boilerplate lands after
  the new key — own-key order is observable. The fast paths that bypassed the
  funnel were the bug source: `fast_fresh_store`, `put_value`,
  `Reflect.set`/`Reflect.defineProperty`, `Object.defineProperty`,
  `Object.assign`'s fresh-destination adoption.
- **Direct own-property reads.** `Function.prototype.bind`'s
  `HasOwnProperty(target, "length")` and ShadowRealm's `CopyNameAndLength`
  read the descriptor without the funnel; the latter was the one fixture the
  sweep caught (`ShadowRealm/WrappedFunction/length.js`). The console's
  name render is the same shape.
- **The compiled chain probe must decline a pending receiver.** A deferred
  function's map is the empty one, so a map-keyed resolution learned on a
  `{}`-shaped receiver could have been served for `f.name`; the probe's
  function edge now loads `props_pending` and falls to the helper. The
  identity cell is safe by construction (nothing is recorded for a pending
  function's boilerplate), but materialization also bumps the receiver's
  generation so no stale cell can survive it.
- **Snapshots.** The walk holds no mutable agent, so a `Record::Function`
  carries the deferral plus the pending name instead of synthesizing the
  properties; the restore materializes from the re-parsed record before
  `define_properties` and before extensibility is applied.

Measured (same-session interleaved pairs, control built from the parent commit
through the build cache; `SLAG_CLOSURE_PHASES` for the phase table):

| probe | before | after | delta |
|---|---:|---:|---:|
| `props` (cycles/closure) | 297.0 / 223.8 / 182.0 | 41.5 / 39.2 / 41.3 | **−85%** |
| closure path total (cycles/closure) | 2629.2 / 2684.6 / 2239.5 | 2368.5 / 2210.1 / 2396.9 | −10% (medians) |
| 1M-closure probe (ms, 9 pairs) | 432.6 / 533.4 / 452.2 / 399.5 / 422.5 / 422.8 / 401.2 / 417.0 | 351.0 / 384.2 / 372.5 / 353.7 / 379.6 / 387.1 / 351.7 / 378.3 | **−12%** min, every pair separated |
| `head-let` (ms, min of 5) | 453.3 | 440.8 | −2.8% (medians 482.2 → 448.6, −7.0%) |
| `evaluation-order` (ms, min of 4) | 247.1 | 249.3 | medians 266.8 → 255.7, −4.2% |
| corpus, sum of per-row minima | 2410.8 | 2360.6 | −2.1% |
| corpus without the two closure rows | 1643.9 | 1643.0 | −0.1% (noise) |

Accept: `cargo test --workspace` (53/53), clippy `-D warnings`, corpus
differential exact in jit, jitless and `--gc-stress` and identical to the
parent build (81/81 rows), `sweep all --accurate` 48,633/48,633 and
`sweep intl402 --accurate` 3,365/3,365.

**The map-keyed chain arm landed without a layout cut (2026-11-15, later).** The
plan had scoped a `kind_is_ordinary` mirror plus a properties-length mirror. Only
the length was needed: the kind discriminator falls out of `props_deferred`
(which `define_fresh` sets only for Ordinary objects, so no exotic — a Proxy most
of all — can reach the deferred gate), and the array gate is
`array_dense != 0 && PROPS_LEN_OFFSET <= 1 && named_version == 0`. Both probes now
read `Agent::member_chain_map_cells` after the identity cell declines, gated on
one of those two shapes plus the new recording precondition
(`resolve_chain_cell` records a map cell only for a name the map does NOT
describe). Results: `array_at` -42%, `array_push` -38%, `push_pop` -33%,
`array_reverse` -28%, `array_indexof` -27%, `array_includes` -22%,
`construct_churn` -8%; corpus `GetMemberName` traffic 52.2 M → 27.9 M.

The length mirror is `Props` (`RefCell<SmallProps>` + a `Cell<u32>` outside the
cell) whose `borrow_mut` guard refreshes it on drop, so the maintenance is
structural. The payload is 432 B inside the 448-byte arena class — the last byte
of headroom (`.notes/perf.md` has the table and the gates).

**What is left** is `opcost/prim_prop` 7.2 M (a primitive string's `length`, whose
native length is not a fixed-offset field), the string-concat pairs ~3.5 M
(`BinarySlow` more than the read), `opcost/num_valueof` 1.6 M, and single digits
elsewhere. `calls/apply_call`'s 7.0 M went away by serving **function receivers**
in both probes (`f.apply`'s read is keyed by the function's object part;
`Function::object` is a second box, so the pointer dispatch must be a branch
through a block parameter — a `select` over both pointers cost `proto_read` 30%).

One negative result stays banked, in corrected form: extending the *identity*
probe to function receivers measured `calls/apply_call` **6.5% slower** in an
earlier session because `is_plain_object` still rejected the receiver before the
probe looked at a cell, so the extension only added decline work. The row needs
the read *served* (the object-part dispatch), not merely admitted.

**A0, function-object creation (~340 ns each).** The measured dominant term in
the corpus's two largest rows (§1.4): three closures per body cost ~1 µs of
head-let's 3.3 µs, against node's 570 ns for the *whole* body.

The phase instrument (`SLAG_CLOSURE_PHASES=1`, `function::phases`) splits the
path into TSC cycles per closure, with an empty timed block measuring the
instrument's own per-phase cost (~31 cycles, subtracted below):

| phase | cycles/closure | what it is |
|---|---:|---|
| `source_info` | 230 | the `SourceInfo` cache lookup + slice/hash for `Function.prototype.toString` |
| `register` | 195 | the `EcmaFunction` record insert into the agent's table |
| `function_new` | 185 | the `Function` object allocation with its prototype |
| `props` | 120–200 | `length`/`name` (+ sloppy `caller`/`arguments`) adoption |
| `record`, `strict`, `body`, `params`, `compile`, `patterns`, `realm`, `context`, `callee_tail` | < 25 each | every cached lookup is cheap |
| `call_register` − `callee_total` | ~70 | the call itself |
| `tail` (caller) | ~0 | the per-iteration-chain install is free when there is no chain |

**A0b is resolved, against its own hypothesis.** The guess was that a quarter of
the cost sat in the by-value `outer_chain`/`per_iteration_chain`/`named` clones
at the caller. Measuring both sides says otherwise: the caller's own work is
`source_info` plus three cached lookups plus ~70 cycles of call overhead, and the
remaining ~40% is *inside* `register_function` between its phase boundaries:
prologue/epilogue on a twelve-argument function, the record move, and the
instrument's own ±50-cycle noise, which is where this method stops resolving.
So the cuts are the four named items, not the plumbing:

- **A0a, the site key is the text's identity (landed 2026-11-14).** The 230
  cycles were dominated by the FNV hash of the function's source slice, and the
  hash could not be dropped on its own: node addresses are reused after a parse
  is dropped, and two parses of same-length sources collide on
  `(address, realm, span)` alone, so something content-derived had to
  disambiguate them. The cut replaces the hash with the *address* of the
  enclosing text, `JsString::Flat` is an `Arc`, shared by every clone, and
  makes the address sound by having every cache that keys on it also store the
  text, pinning the `Arc` so its address cannot be recycled (`source_site_identity`
  and `SiteSource` in `function.rs`). The forms with no stable base (`Small`,
  and a rope until it is materialized) keep the hash. All four site caches take
  the identity, `SOURCE_INFO_CACHE`, `FUNCTION_BODY_CACHE`,
  `ARROW_BODY_CACHE`, `ACCESSOR_BODY_CACHE`, and `SourceInfo::key` carries it.

  Measured on the head-let fixture with `SLAG_CLOSURE_PHASES`: `source_info`
  **273.0 → 183.9** cycles per closure and `total` **1890.8 → 1783.9**
  (-5.7%); `body`, `props` and `patterns` moved inside the instrument's
  ±50-cycle band. The wall clock does not resolve it in this window (that row
  spans 337-602 ms within one binary), so this is an isolated win, not a row
  claim. What is left in the phase is `enclosing_source` (a walk of the
  execution-context stack plus a `JsString` clone per closure) and the
  `SOURCE_INFO_CACHE` probe.

  **The resolution is now memoized per site** (2026-11-15): a closure created in
  a loop re-enters `shared_source_info` with the same text and span every
  iteration, so an eight-entry direct-mapped memo keyed by the running context's
  text address and the span returns the `Rc<SourceInfo>` without the stack walk,
  the clone, the key build or the cache probe. The key is the identity the cache
  already uses, and each entry holds the `Rc`, which pins the text exactly as the
  cache does, so a recycled address cannot serve the wrong slice. `source_info`
  **183.9 → 31.7** cycles per call and `total` **1799.7 → 1639.0** (-8.9%); the
  head-let row goes 331.7 → 322.6 ms on the mean of three interleaved pairs
  (minima 318.2 → 314.7), which is the same direction but below this host's
  resolution. 676 `built-ins` `*toString*` fixtures still pass, which is what
  would catch a wrong slice.
  already carries its source, so it can carry the site key with it: that is
  the next item.
- **A0c, `props` and the record insert** (120–200 + 195): property adoption
  behind a pending flag like `prototype`'s is the remaining safe item. The
  record insert was attempted the obvious way and **measured worse**:
  `ecma_functions` is an `IdMap<EcmaFunction>`, so boxing the value
  (`IdMap<Box<…>>`, pointer-sized inserts and rehashes) was the hypothesis;
  it took `register` from 195 to **326 cycles**, because the box is one more
  allocation per closure and the saved move is cheaper than the malloc. So the
  insert's 195 is not the move; a dense `Vec` indexed by the (sequential) id is
  the shape that would remove both, and it is a table-wide change rather than a
  cut.
- **A0c is also closed for `props`.** `set_function_properties_batched` already
  adopts a pre-forked shape with a vector-free field write (Cut 67), so the
  120–200 is a boilerplate-map lookup plus one adoption, not a sequence of
  defines. Deferring `name`/`length` would mean the shape *lacks* those keys at
  creation and gains them on first observation, the same trick `prototype`
  plays, but `prototype` is safe because it is reached through one
  materialization site, while `name`/`length` are ordinary own data properties
  that the boilerplate fork and every property read assume are present. That is
  a layout change, and the layout is a measured trap (§7's `JsObject` note):
  not worth ~150 cycles of one row's closure path.
- **A0d, `function_new`** (185): one object; at ~44 ns per empty object this is
  ~1.5 objects' worth, so there is not much here beyond avoiding the allocation
  entirely (`A0c`'s fold would do that).

**Accept:** the closure micro (§1.4) falls; both fixtures fall with it; the
corpus differential and `--gc-stress` stay exact (a closure is a heap object with
an environment edge, the rooting traps in §7 apply).

**A1, the corpus's biggest body cannot enter the tier (landed 2026-11-14).**
`__t262Body` in the `head-let` fixture was 101 steps and bailed on the
`for (let x in obj)` per-iteration environment family (`EnterPerIteration`,
`PerIteration`, `LoadPerIteration`, `StorePerIteration`, `LeaveBlock`) plus
`TypeofTop`. All six now lift as helper calls (`PerIterEnter`/`PerIterNext`/
`PerIterLoad`/`PerIterStore`/`LeaveBlock`/`TypeofTop`), and the same cut found a
real modelling bug: `TemplateSub` is a pop *and* a push, but `stack_delta`
charged it −1, so every substitution drifted the block depth down and any body
with a template before a conditional was refused as `Malformed("stack
underflow")`, that is what kept the template fixture's 40-step body out.

Measured outcome: **parity, not speed** (1.002–1.020× against `SLAG_OPT=0` over
three five-round runs on both fixtures; corpus 2.62–2.70× node, differential
exact). Two more bodies are compiled and the last anonymous/malformed refusals
are gone; the two fixtures are still allocation- and call-bound, which is A0
and D5/E, not coverage. The one refusal left in either fixture is
`UpdateContextSlot`, the deliberate one the notes' test pins.

**A1b, the register body's member leaf ops (S3) and stores (S4).** Lift
`GetMemberNameLocal`/`GetMemberComputedLocal` to `MemberLoad`/`MemberCellLoad` +
`MemberGuard` (read-only first, as S3 was scoped), then `BinAccPop`, `UpdateAcc`
and the member stores. Probe: `JIT_DUMP_STEPS` bodies per row, before/after, and
for each newly lifted body its helper census against the per-step body's.
**Accept:** every newly lifted row ≤ its per-step time; corpus differential
exact; the `installed_jit_*_inlines` tests pass without forcing.
**Stop rule:** a newly lifted row >2% slower means a missing parity port, port
it before proceeding, never carry a regression into the next cut.

**A2, the hoist probe: blocked on the type prover (attempted and measured
2026-11-14).** The largest coverage bucket (§3, six rows) is not a missing step
but a deliberately refused terminator: `emit_fused_test` models
`HoistMemberGuard`/`HoistGlobalGuard` as "always miss", which measured
`objects/own_read` **+571%** and the `globals/*` rows ~**+60%** each against
`SLAG_OPT=0`, so it refuses instead.

The probe itself is easy to model and was implemented and measured twice. Attempt one: an `Op::HoistGuard` lowered to one helper call (`hoist_guard`, running the interpreter's own probe through a shared `Vm::hoist_member_guard` / `Vm::hoist_global_guard`), branching to the guarded copy on a hit and the general copy on a miss. Every one of the six rows lifted, and every one got **slower**: opt / per-step, min-of-5, same binary, `own_read` 3.52, `nested_read` 1.54, `object_read` 1.44, `declarative_read` 1.44, `proto_read` 1.08, `obj_prop` 1.00 (the last also returned a wrong value). Attempt two added slot-precise promotion, the probe's slots carried in `Imm::Hoist` and flushed before it, so the loop accumulator stays an SSA candidate instead of forbidding promotion of the whole frame: 1.36–3.62× still, `obj_prop` at parity only because its int path now refuses. Both attempts are reverted.

The blocker is the body's shape, not the probe, and it is now precise:

- A guarded body contains **two copies of the loop** (guarded and general)
  writing the same frame slots, and `narrow`'s slot prover is whole-body and
  flow-insensitive. The general copy's imprecise store is evidence against the
  guarded copy, so the guarded copy's `+` lowers to the branchy checked path
  (or worse, `binary_slow`) while the per-step tier gets a bare f64 add, the
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

So A2 is two cuts, in this order: **(a)** make the type facts region-scoped, the
guard's payload applied to the blocks its hit edge dominates (a forward
must-analysis: intersect the facts over each block's predecessors, with the
hit edge adding the guard's own), which also fixes the lift discarding
`BinStoreNum`'s proof, then **(b)** re-land the probe model: an
`Op::HoistGuard` (`Imm` = the step index and the slots it addresses,
`Type::Bool`, `Effects::call()`) lowered to one `hoist_guard(ctx, step)` helper
call that returns the hit and has already written the hidden slots, the lift
arm returning `(hit, false)` so a hit falls through to the guarded copy and a
miss branches to `jump_target`, and `mem2reg` flushing a promoted slot before
the probe that names it. **Accept:** every one of the six rows ≤ its per-step
time; `HoistGuard` drops out of the refusal census.

**A3, name the emit refusal (landed 2026-11-14).** `step_name` now names every
family that has appeared as a corpus first blocker (`HoistGuard`, `Switch`,
`Try`, `Generator`, `UpdateContextSlot`), and the second-pass terminator refusal
(`emit_fused_test`) reports through `report_refusal` like the per-step path, so
`opt-refuse:` names the exact step and `opt bail` names its family. The
anonymous `Step("step")` bucket is gone from the corpus; the probe's own
history is the reason it existed (`report_refusal` had one call site, and
`step_name`'s catch-all was the only name for a try/switch/generator step).

**A4, `SwitchDisc` and the register-body leaf ops come next**, in whichever
order the refusal census shows the most rows behind. Neither has a measured
pessimization attached yet, so both are parity cuts, not re-enablers.

**B (residue).** The census says the median row has 1 avoidable conversion per
iteration, so the phase as written is not worth its risk. What survives:
- **B1, the `compute`-caused `Int` widens** (`int` median 1, max 7): each is an
  op with no int lane. Only worth doing per-op, with the census row as the
  before/after, and only on the six rows with ≥5.
- **B2, the `Float` boundary.** Already correct (`canon_double` at the
  materialization, one funnel); keep the census's `raw f64 without Canon`
  invariant at zero. It is the cheapest check that a later pass did not break
  the funnel.
- **B3, a `Bool` lane** (i1/i8) is the one representation change with a
  plausible payoff (`Lt → Branch` and every `JumpIfTrueKeep` chain), but the
  census charges those to `compute` at ≤2/iteration on the median row: measure
  before building it.

**C1, keep the refusal, and make it explicit.** Today `non_heap_slots` is the
policy but nothing states it as one: a heap-typed slot is never promoted, so a
heap value lives at level 1 and is rooted. Document it at `promote_frame_slots`
and add the test that a promoted slot's *load* after a call re-reads the frame
(the slot-observing-call trap, §7).

**C2, the stack map.** Cranelift user stack maps per safepoint; the collector
walks them for the PC's code range. Prerequisite for any cut that keeps a heap
reference at level 2 (a generalized E, an inlined callee's context), and now
for D5 (§4). **Accept:** `--gc-stress` and `--gc-verify` exact on every row whose
body holds a heap value across an allocation; a targeted test that allocates
under a live register-held reference with `--nursery-stress`.

**Slice 1 landed 2026-11-14: the mechanism.** `cranelift-codegen 0.134.3` has
user stack maps, `DataFlowGraph::append_user_stack_map_entry` attaches entries
to a safepoint instruction and `MachBufferFinalized::user_stack_maps` returns
them finalized, so `assemble` now extracts every map into `Compiled::maps`
(`StackMap { offset, entries: [StackEntry { sp_offset, width }] }`), keyed by the
safepoint's code offset with the SP-relative offsets already resolved by
regalloc, and `SLAG_TRACE=stackmap` prints them. A test compiles a hand-built CLIF
function whose call carries one entry and asserts the round trip, so the API,
the finalized offsets and the extraction are pinned on this target.

Nothing attaches a map yet: the tier's own lowering still produces empty vecs,
because deciding what to spill is liveness (slice 2) and walking the maps from
a collection is the collector registry plus the PC lookup (slice 3). A body with
no maps compiles to exactly the code it did before.

**Slice 2 landed 2026-11-14: liveness.** `opt::live` answers the producer's
question, `safepoint_roots` runs a backward data-flow fixpoint over the SSA CFG
(a successor's `live_in` flows into this block's exit; loops need the fixpoint)
and reports, per call, the values live *after* it, excluding the call's own
result and filtered to the types that may be a GC reference (`Unknown`, `Object`,
`String`, `Number`, `Hole`; `Int`/`Float`/`Bool`/the immediates cannot be).
`SLAG_TRACE=live` prints them, which is how the numbers below were read.

Two things the first real check showed. The set is **conservative and large**:
a probe loop body with four calls reports the same ten live values at every one
of them, because a frame load is `Type::Unknown` and untyped means heap-typed.
That is the honest cost of the policy, and it is why slice 3 cannot simply spill
everything at `Type::Unknown`: the typer's guards have to narrow the set first,
or the store traffic will exceed what the roots are worth. Second, the analysis
belongs at the call sites the lowering *emits*, not at the ops: `is_safepoint`
naming `Op::Call | Op::Intrinsic` is a claim about `emit_leaf_call` that must be
re-checked whenever a new call-emitting op lands.

**Slice 3 (landed 2026-11-14): spill and declare, gated.** `SLAG_STACKMAPS`
spills the whole root set into sized stack slots, each initialized to
`undefined` in the prologue and rewritten at every definition and block
parameter, so a slot always holds a valid `Value`, and names every slot in a
stack map at **every** safepoint in the body. Threading a root set through the
lowering's call funnels (`call_helper`, the four leaf-lane `call_indirect`s,
`call_slow`) was the alternative; naming the full set at each call instead
over-approximates the roots, which is sound because the slot only ever holds a
value that is already dead at that call. Entries are attached after lowering by
walking the CLIF layout for `opcode().is_safepoint()`, so a new call-emitting op
needs no change here.

Measured on the probe bodies: 71 safepoint maps, 10 entries each, and the rows
do not move, `d4` 2.07-2.09 → 2.09-2.16 ms, `e4` 20.5 → 20.7-21.5 ms, because
those ten candidates are defined *outside* the loop, so the spill stores run
once per body rather than per iteration. A body that redefines them inside the
loop would pay per iteration and is not measured; that is the case to check
before the set is narrowed. Results are identical with the gate on and off over
`opcost` in both `jit` and `jitless`.

Still off by default, and the maps are still not **walked**, and that is where
the arc stops, for a reason outside this crate.

**Slice 4 is blocked on a native-code convention** (found 2026-11-14 reading
Cranelift). A map's offsets are SP-relative: `sp_to_sized_stack_slots()` is the
frame's `outgoing_args_size`, added to each sized slot's frame offset, so the
base is `rsp` at the safepoint. CLIF cannot produce that value, `stack_addr`
addresses a sized slot, and there is no `get_sp`/`get_fp` instruction in
0.134.3. So a helper called from compiled code cannot be handed the base its
callee `rsp` is offset from by the return address and its own prologue, and no
portable Rust can recover it. The options are a per-target assembly shim that
captures `rsp` at the call and passes it as an extra argument to every helper
(the four-file mirror, plus x64 and aarch64 shims), or a frame-pointer
convention with the same shim to read it. Both are an ABI change, and neither
belongs inside this cut.

**Consequence: D5 does not need any of this.** The fused loop's roots, the
array and the callback, can live in traced `vm.stack` storage (the
scratch-slot variant this note rejected earlier as the fallback), which the
collector already walks: no map, no SP, no shim. C2's *register* case is the
only thing the shim buys, so the order returns to **D5 via traced slots first**,
with the shim only if a later cut genuinely needs a heap value in a register
across a safepoint. Slices 1-3 stay as the producer half, gated off, and are
the thing to finish if that cut appears.

**D5, the callback lane.** `array.forEach(cb)` and friends re-enter the generic
call funnel per element. Sized 2026-11-14: `opcost/array_for_each` 22.4 ms
against its hand-written twin `array_for_each_js` at 3.55 ms, **6.3×**, and the
same shape in `map_for_each`/`typed_array_for_each`, so ~50 ms corpus-wide.

**Split 2026-11-14** with a four-file probe (100k outer iterations each, min of
three):

| probe | jit | jitless | per unit |
|---|---:|---:|---|
| the outer loop plus `a.length` (base) | 0.585 | 2.77 |, |
| four compiled direct calls `cb(a[0..3])` (the twin) | 2.15 | 14.0 | **3.9 ns/call** |
| `[1].forEach(cb)` | 8.8 | 9.65 | **~41 ns fixed** per call |
| `[1,2,3,4].forEach(cb)` | 21.0 | 18.1 | 41 ns + **40.7 ns/element** |

Two things follow, and the second corrects this plan's premise. First, the row
is ~90% lane (21.0 against the twin's 2.15) and splits ~20/80 between a fixed
per-`forEach` cost and a per-element one; the per-element 40.7 ns against a
compiled call's 3.9 ns is **10.4×**. Second, **the callback is already compiled
and compiling it is not what the row is paying for**: `CallbackVm::call` →
`run_leaf_entry` → `try_jit_leaf`, so the machine code runs, and `forEach` is
*slower* with the JIT than without it (21.0 against 18.1), because `run_jit_leaf`
costs the code-cache `lookup_info`, the `leaf_gen` bump G15 requires at
`jit_depth == 0`, the `LeafEntry` clone, `running_context`, `rebind`, `retarget`
and three stack pushes per element, while the interpreted leaf is a three-step
body. So there is nothing to win by shaving the Rust dispatch either: its floor
is the interpreted-leaf cost the jitless column already measures.

**The cut is therefore a fused loop in the IR**, not a better callback entry: an
`opcost`-shaped `forEach` lowered to a counted loop whose body *is* the callback
(inlined, so the 40.7 ns boundary disappears rather than shrinking), with the
length read once before the loop per spec 23.1.3.14. `E` then has a body big
enough to contain the allocator and the consumer, which is the only route to the
two fixtures (node eliminates their per-iteration closures outright). **Accept:**
`array_for_each_js` and `map_for_each` fall, differential exact,
`JIT_MAX_COMPILE_STEPS` body sizes recorded.

**Landed 2026-11-14: the lane stops choosing the slower lane.** The premise
correction above has a direct use. If a small leaf body runs faster through the
interpreter than through the compiled lane, because from a Rust caller the JIT
entry point pays `lookup_info`, the `leaf_gen` bump and the whole `run_jit_leaf`
prologue per element, then the callback lane should not enter it.
`CallbackVm::call` now runs a leaf of at most 8 steps through
`run_leaf_entry_interpreted`, which skips `try_jit_leaf` entirely. Measured,
three interleaved pairs, same session:

| row | before | after |
|---|---:|---:|
| `opcost/array_for_each` | 21.0-22.2 | **14.7-15.0** |
| `opcost/map_for_each` | 22.4-26.3 | **15.3-16.1** |
| `opcost/typed_array_for_each` | 25.1-28.7 | **17.2-18.3** |
| `opcost/array_for_each_js` (the hand-written twin, control) | 3.46 | 3.46 |

−30% on the three rows, the control unmoved, and all 81 rows identical before
and after in both modes. It lands *below* the jitless column (18.1) because the
caller's loop is still compiled and only the callback changed lanes. What is
left is the 4.1× to the twin (14.7 against 3.56), which is the fused loop's job.

This corrects a claim made earlier in this note, "there is nothing to win by
shaving the Rust dispatch either". That was true *of the compiled lane*; the win
was in not entering it at all.

**Landed 2026-11-15: `forEach` is an intrinsic, and its loop runs in place.**
`Intrinsic::ArrayForEach` reuses the mechanism's three free pieces (recognition
by member name, the callee-identity guard, the `do_call_fast` fallback), and
`do_call_intrinsic` runs the loop on the invoking `Vm`: the element walk is the
builtin's own (`dense_own_element`, then the chain via `has_property` /
`get_property` for a hole), and the callback runs through the leaf lane on that
Vm instead of a pooled one, so the builtin dispatch, `to_object`,
`length_of_array_like` and the pool round trip are gone. Measured on
`opcost/array_for_each`, jitless: 19.0-21.1 → **13.7-14.2 ms**. The `map_for_each`
and `typed_array_for_each` rows are untouched (they use their own methods), and
the corpus differential is exact in both modes, with 313 `built-ins` fixtures
matching `*forEach*` passing.

**The jit column does not move, by design.** `emit_intrinsic`'s arm for the new
kind jumps to its shared `slow` edge (the general call), because a machine-code
loop is the remaining piece; the alternative, leaving the kind unhandled, would
hit the `Unsupported` return and drop every body containing `.forEach(` out of
the optimizing tier, which is the exact row's hot caller. So the tier still
compiles the caller and the call is what it was.

**The fused loop's integration point is the `Intrinsic` mechanism** (mapped
2026-11-14), and it removes most of the plumbing this note assumed. `Intrinsic`
already gives the three things the loop needs: the *recognition* (`try_compile_intrinsic`
matches a member call by property name, so `.forEach(` reaches it with no lift
pattern and no bytecode change), the *identity guard* (`do_call_intrinsic`
compares the resolved callee against `realm.intrinsics.intrinsic(kind)` and
returns `false` to fall back to `do_call_fast`, a shadowed `forEach` therefore
cannot take the fast path), and the *fallback*. `intrinsic_bits` publishes a
kind's identity generically (`for (slot, kind) in bits.iter_mut().zip(INTRINSICS)`),
so the JIT's `emit_intrinsic` guard needs no change either.

Two things to watch. The compiler must restrict the shape (`argc == 1`, as
`ArrayPush` restricts its single-element form), and **`emit_intrinsic`'s inner
`match` returns `Unsupported` for an unhandled kind, which drops the whole body
out of the optimizing tier**, so a new kind needs a JIT arm in the same change
as the interpreter arm, even if that arm only routes to the shared slow path.
Otherwise every body containing a `.forEach(` loses the tier, which on this
corpus row is exactly the hot caller.

The loop body itself is `for_each`'s element walk (`dense_own_element`, else the
chain via `JsObject::has_property` + `get_property`) with the callback run on
*this* Vm instead of a pooled one, the leaf lane `CallbackVm` uses, minus the
builtin dispatch, `to_object`, `length_of_array_like` and the pool round trip.

**The inline pass has no input by default, and that is why nobody could
measure it** (2026-11-15). `record_call` is called from exactly one place, the
interpreter's call step, and both probes return early unless `SLAG_FEEDBACK` is
set; the compiled tier never records. So a body compiled before its sites
specialize never specializes at all, and `pass::inline` (which requires
`IcState::Specialized` plus a callee id) sees nothing. Reading it with a new
`SLAG_TRACE=inline` channel shows the shape: the hot `__t262Body` body reports
**101 call sites, 0 monomorphic**, while a colder body reports 33 sites with 1
monomorphic and splices 30 steps.

With `SLAG_FEEDBACK=1 SLAG_INLINE=1` the head-let row does move: 317.7 / 318.6 ms
to **304.5 / 306.8** ms over two pairs, about 3.7%, from that one 30-step splice.
That is the whole of trial inlining's measured effect today, and the reason is
the missing recording rather than the splice: the body that holds 45% of the
corpus gap is the one with zero monomorphic sites.

The next cut is therefore to feed the probes from compiled code, either by
recording on the compiled call path under the same gate or by specializing a
site before its body is compiled. Both are cheaper than growing the splice, and
neither can be measured until the recording exists.

**The bodies that matter have no feedback store at all.** `sites_from_feedback`
now reports what it found under `SLAG_TRACE=inline`, and the answer is emphatic: of
the five lifted bodies in the head-let fixture, four print **no store**, meaning
`record_call` never ran for them, and one prints `1 call records, 1 with a callee
id`. `record_call_site` has exactly one caller, the interpreter's `CallFast` arm,
and a body that contains a loop compiles on its first consult (Cut 69), so it
never runs interpreted at all. `__t262Body` has the `for in` loop, which is why it
reports 101 call sites and nothing recorded. The only body that records is a
33-step, loop-free one, and that is the one that splices.

So the inline pass cannot see the bodies it was built for, and neither the
threshold nor the splice size is the problem. Recording must move to the compiled
call path, where `Op::Call` already carries the step index in `Imm::Int`.

**A loop body now waits under the probe** (2026-11-15), which is the cheap half
of that: `lookup_info` lets a loop body bypass the compile threshold only when
the feedback probe is off, so under `SLAG_FEEDBACK` a loop body runs interpreted
like any other body until the count is reached, records its calls, and compiles
with feedback in hand. The probe's own gate keeps production behaviour
unchanged. `__t262Body` goes from **0 to 10 call records, 9 monomorphic**.

Measured on head-let, two pairs: 334.0 / 335.8 ms without the probe, 312.9 /
311.7 ms with it, about 6.7% (it was 3.7% when only the loop-free body could
splice). The 81-row differential is exact with the probe on, and 1161 runtime
tests pass.

What has NOT changed: `try_inline` still refuses those nine sites
(`spliced=false, calls_left=1`), so the visibility is not yet the splice.

**And the refusal is a shape rule, not a resolver failure** (2026-11-15). With
the reason reported, the answer is `why=no candidate call` while `calls_left=1`:
the candidate scan skips any `Op::Call` whose `result` is `None`, and every call
in this body is an expression statement (`assert.sameValue(...)`, `fns.a()`), so
none of them is ever a candidate. Nine monomorphic sites, and the splice cannot
see one of them.

That is a bounded defect rather than a design limit: the splice builds a
continuation block for the call's result and rewrites its uses, and a call whose
result is discarded has neither. The shapes that need it are everywhere (every
assert-style body, and the callback statements in the corpus), so it is worth
doing before anything about callee shape or budget, which is what the next
refusals will be.

**The fused array loop is blocked on C2, which the ordering did not price**
(found 2026-11-14 reading `opt_lower.rs`). A fused loop must hold the array and
the callback across every `emit_leaf_call` in its body, and that is a heap
reference live at a safepoint
inside compiled code, exactly what C1's policy forbids. The tier's answer today
is the refusal: a heap value is never promoted, so it lives at level 1 in a
frame slot. `Op::Call` can hold `callee`/`this` in SSA only because it needs
them *up to* the call, never after it, and the helper roots its own arguments
before allocating; a loop needs them after, across N calls and N allocations,
where the collector cannot see a register. So they must live in traced storage
for the duration of the loop, and there is nothing to use: the opt tier has no
scratch-slot allocator (the frame layout is the body's own slots), the calling
convention places the callee's frame above `args_ptr + argc*8` so the working
region cannot be borrowed, and reloading the operands' *own* locals per
iteration is wrong, the spec captures the callback value once, and the body
may reassign the local.

So §5's "C2 before any level-2 heap value ... nothing above requires it,
because the tree's policy is refusal" is false for D5 as written: **D5 is the
first cut that requires it**, and so is E, whose scalar replacement keeps a
field that holds a heap value. The order becomes C2 → D5 → E, and D5 is not
the next cut. The alternative to C2, two scratch frame slots per fused loop,
is a frame-layout change (the layout is a measured trap, §7), so it is the
fallback, not the plan.

**E (residue), escape analysis beyond literals.** `pass/scalar.rs` already
scalar-replaces a whole `ObjectFast`/`ArrayFast` whose reads are all
constant-keyed. Generalizing it needs a body big enough to contain both the
allocator and the consumer (i.e. D5-scale inlining) and C2 for a field that
holds a heap value. Probe: `--gc-trace` allocation count per row. **Stop rule:**
if the surviving allocation count barely moves, go back to A, not deeper into E.
Do not re-open the `JsObject` layout for allocation speed: shrinking it is a
measured non-win (`{}` −1.4%, `[]` +32%, corpus +2…5%).

**F1, `clobberize`.** Effects are over-approximated (`default_effects` gives
bit ops and comparisons `Effects::call()`). This is now the *next* thing with a
measured consumer, unlike on 2026-10-09: `mem2reg`'s slot promotion, `licm` and
`cse` all run, and the census reports the conversions an over-approximation
forces. Order it after A1/A2 (coverage) and D5 (body size), and measure the
census before and after the narrowing.

**F2, GVN/load elimination.** LICM and CSE have landed; the remaining read rows
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
- **C2 before any level-2 heap value, which now means before D5 and E.**
  Nothing in A requires it, because the tree's policy is refusal; but D5's fused
  loop holds the array and the callback across the calls in its body (§4 D5 has
  the argument) and E's scalar replacement holds a field, so both pay for the
  stack map. C2 is the next cut after the register work, not a later one.
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

Still live from the earlier notes: **`JIT_MAX_COMPILE_STEPS`** (128/1024:
inlining grows bodies and an oversized one silently returns to the interpreter);
**GC roots in optimized frames** (level 2 is invisible to the conservative scan,
and a promoted *heap* slot unroots it, the barrier-UAF class); **resume
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
  no blockers at all. Read all three before calling a bucket a coverage gap.
  The first version of this session's census called nine rows "shape refusals"
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
  the hoist work, a separate, cheap cut.
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
  `Malformed("stack underflow")`, a latent lift bug previously masked by the
  earlier bail. Both are refusals (the body still runs per-step), but the second
  means the lift's stack model is wrong somewhere in that body's shapes; treat a
  reason change after a widening as a new work item, not as "still refused".
- **An instrument's own cost is a phase.** Two `_rdtsc` reads plus an atomic add
  measured 31 cycles, i.e. ~20% of a 160-cycle phase, and 12 phases put ~370
  cycles inside the `total` they inflate. Time an empty block beside the real
  phases and subtract it, or the split will read as nine equal slices.
- **A step's stack delta is the block-depth model, not a stack trace.**
  `stack_delta` feeds `stack_depths`, which sizes merge-block parameters and
  checks each conditional's edge for underflow. A pop-plus-push step charged as
  a pure pop (`TemplateSub`, −1 instead of 0) does not fail where it is
  written: it drifts every later join by one per occurrence and refuses the
  body as `Malformed("stack underflow")` at the first conditional after it. The
  drift is uniform along a path, so joins stay consistent and nothing
  miscompiles, the symptom is always a refusal, and the reason names a step
  that is not the culprit.
- **A hypothesis that names a chunk is not a measurement that locates it.** The
  missing quarter of the closure path looked like caller-side `Rc` clones;
  instrumenting both sides showed ~70 cycles of call overhead and put the rest
  *inside* the callee, between phase boundaries. Instrument the other side
  before designing the cut, this guess was simply wrong.
- **Boxing an id-keyed record map does not pay.** The `register` phase read 195
  cycles and the record is ~30 fields, so `IdMap<Box<EcmaFunction>>` looked like
  the fix; measured, it is **326**, the box is a malloc per closure and the
  move it removes is cheaper than that. The insert's cost is the table, not the
  value's size; removing it means a dense `Vec` keyed by the id, not a box.
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
3. **Whether `Op::Call` keeps a general lowering.** Yes, the splice falls back
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
