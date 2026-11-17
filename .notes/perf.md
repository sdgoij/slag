# Performance

The standing record for speed work: where the engine is now, what it holds every
change to, and the one plan that carries the rest. This file was restarted
2026-11-14 when the register/materialization arc closed at parity; the history
before it, the dated journal, the failed-experiment register, and the per-arc
records, is at `.notes/archive/perf/` (indexed in its `README.md`).

## Where we are (2026-11-14)

Corpus, 81 workloads, `node tools/corpus/bench.js` medians of three runs on one
machine, parity exact across all four engine/mode combinations:

| Family | n | JIT gap | Interpreter gap |
|---|---|---|---|
| arrays | 6 | 9.60x | 3.21x |
| builtins | 7 | 21.00x | 14.72x |
| calls | 6 | 8.84x | 3.04x |
| control | 5 | 12.45x | 3.21x |
| globals | 4 | 2.19x | 0.74x |
| language | 3 | 5.14x | 4.56x |
| objects | 7 | 8.36x | 1.86x |
| opcost | 38 | 11.60x | 3.21x |
| strings | 5 | 9.48x | 4.15x |
| **All** | **81** | **10.75x** | **4.12x** |

`README.md` ("Against V8") carries the per-workload sample rows and the caveats
that make these numbers readable: `builtins` is the noisiest family and its
ratio is set by the two async rows against a sub-millisecond V8 baseline, and
the `globals` family is a controlled experiment rather than a workload. The
JIT-against-interpreter tables are there as well.

Two statistics that are not interchangeable: the corpus's own `mean-jitGap`
(above) is an unweighted mean of per-row ratios and is dominated by
sub-millisecond V8 rows; a total-milliseconds ratio over the same run is
roughly a quarter of it. Quote one or the other, never a mix.

## 2026-11-15: two register-lane refusals closed

Three cuts. The first two are refusals that had been silently costing whole rows; the third is a probe removed from the closure path.

- **The loop counter's entry constant** (`pass/loop_counter.rs`). The register-body lane emits `IntAdd`/`IntSub` for a counter its int plan proved, but the entry constant (often shared, as in `s = 0`) was still a `Number`, so `normalize_param_types` typed the loop parameter `Number` and `opt_lower` refused the body at `opt:int-operand`. Six rows ran the per-step tier on this alone.
- **The register body's accumulator named read** (`opt/lift/mod.rs`, `LeafOp::GetMemberName`), the other half of the `RunRegBody` blanket refusal, which also blocked six rows. Refused leaf ops are now named individually (`leaf_op_name`); the only register-body blocker left in the corpus is one row's `leaf:BinRemConst`.

Same-session, interleaved, min-of-15 isolated: `objects/warm_store` 41.3 to 24.1 ms and `objects/compound_assign` 49.9 to 30.9 (both bodies now lower through the tier), `opcost/prim_prop` 1.73 to 1.16, `objects/many_objects_read` 23.0 to 18.7. Corpus total, whole-corpus A/B over five alternating rounds against the pre-cut tree: **2623.5 to 2534.4 ms (-3.4%)**, 81/81 identical results.

- **An inlined prototype-chain read** (`compiler.rs`, `cells.rs`). The interpreter has cached prototype resolutions since Cut 23/75, but the cache is inside `get_member_name`, so compiled code paid the helper on every prototype read. Re-validating the identity-keyed cell in machine code (with the helper as the decline) takes `objects/proto_read` 58.0 to 30.6 ms (`SLAG_CHAIN_PROBE=0` against unset, one binary, min-of-9) and drops the corpus's `GetMemberName` traffic 82.8M to 71.5M. The register-body `LeafOp` lanes had their own read arms, so the step arm alone reached nothing, and the same seam showed that the +34..73% "regressions" the cross-binary A/B reported on three 1-2 ms array rows were layout (they are -2.9%, +0.9%, -1.5% within one binary).
- **The map-keyed chain probe is scoped in the plan, not landed.** It needs proof the receiver owns no `name`, and neither available substitute is sound: a map id does not imply it (a vector-only own property can sit on the shared canonical empty map, which is why the interpreter re-checks), and every exotic kind is created with that same empty map (so a Proxy can hit a plain object's cell). It is a `JsObject` layout cut — a kind mirror plus a properties-length mirror — not an emitter cut. Extending the *identity* probe to function receivers measured `apply_call` 6.5% slower and was reverted.
- **Inline truthiness in the optimizing tier** (`opt_lower.rs`). `Term::Branch` and `Op::Not` called `Helper::ToBooleanSlow` for any condition not typed `Bool`; the per-step tier's `emit_truthiness` was never ported. Corpus-wide `ToBooleanSlow` goes 10.7M to 0, and layout-neutral that is `head-let` -3.7%, `evaluation-order` -2.1%, `map_churn` -6.2%. The first attempt added the object/function/symbol tags to the fast path and measured *nothing* (and +8.7% on `object_keys`); the inline sequence has to stay as short as the call it replaces.
- **The per-iteration chain** (`function.rs`). A closure creation looked its own fresh record up again after registering it, only to install the creation site's per-iteration chain. Carrying the chain into `register_function` removes that probe: 3347.7 to 2501.0 TSC cycles per closure on a 3-captured-closures loop, and -5.7% on that loop (min-of-15, one aligned binary pair). It must be installed after `shared_compiled_body`, because the compile reads it and the compiled body is shared per site.

### The helper re-entry census (2026-11-15)

`python tools/corpus/helper_rows.py` runs one row per process with `JIT_HELPER_STATS` and attributes the histogram, which is the census that replaced guesswork after the closure-path cut landed and `head-let` did not move. Corpus-wide totals, largest first:

| helper | calls | where |
|---|---:|---|
| `GetMemberName` | 82.8 M | `objects/proto_read` 28.0 M (essentially all of it), `set_churn` 8.4 M, `apply_call` 7.0 M, `map_churn` 6.3 M, `prim_prop` 6.0 M |
| `CallSlow` | 39.6 M | `recursive_fib` 14.9 M (every `fib` call), `set_churn` 4.2 M, `map_churn` 2.1 M |
| `BinarySlow` | 12.7 M | `coercion_concat` 6.3 M, `head-let` 4.2 M |
| `ToBooleanSlow` | 10.7 M | `set_churn` 4.2 M, `map_churn` 2.1 M |
| `CertifiedCall` | 10.3 M | the certified lane working as designed |
| `UnarySlow` | 7.8 M | `head-let` 4.2 M + `evaluation-order` 3.5 M (`!is(...)` in `sameValue`) |

`head-let`, the biggest row, makes ~58 helper calls per iteration (`CertifiedCall` 9, `BinarySlow` 6, `UnarySlow` 6, `LeafCallProbe` 3, `LoadIdent` 3, `CreateFunction` 3, `AssignMemberName`/`AssignMemberComputed`/`FastArrayElementWrite` 3 each, `LoadPerIter`/`StorePerIter`/`PerIteration` 3 each, `ForInNext` 4, `TypeofTop` 3, `EnterPerIteration`/`InitContext`/`ObjectBegin`/`LeaveBlock`/`ForInBegin` 1 each). That surface, not closure creation, is what the row is bound by: a 25% cheaper closure path moved it 0.4%.

Three findings that size the next cuts:

- **`GetMemberName`'s traffic is prototype reads whose cache is *inside* the helper.** `proto_read` (one stable receiver, two prototype data properties, 4 M reads) takes 28 M helper calls because the compiled read probes only the own-property cell; `member_chain_get`'s identity- and map-keyed chain cells (Cut 75) then serve the body cheaply, but the call itself is already paid. Inlining the chain probe into the compiled named read is the cut, and the obstacle is the link walk being a variable-length loop.
- **`ToBooleanSlow` in a lifted body is a missing port, not a workload.** The per-step tier has `emit_truthiness` (numbers, tags, and a slow arm); `opt_lower`'s `Term::Branch` has no inline path at all for a non-`Bool` condition, so `if (st.has(k))` pays a helper per iteration even though the answer is a `Bool`. `set_churn` is 600 k such calls per `bench()` in a 49 ms row.
- **`recursive_fib`'s 14.9 M `CallSlow` is the self-call lane's own cost, not a funnel miss** (`JIT_LEAF_TRACE` says `leaf-lookup-miss` and the self path runs inside `call_slow`). 31 ns per self call against node's 4 ns lives in `self_call_inline`'s frame work, which the plan's §20 already records as a bloat-sensitive area.

Two things this arc re-confirmed about measuring here. A whole-corpus single pass per side is not an instrument: rows whose generated IR was *identical* moved by 30-40% between the two binaries, so the per-row isolated, many-round protocol is what decides a row. And `JIT_DUMP_IR` diffs are the way to establish which rows a tier change can even touch: `python` diffing the dump over all 81 rows (heap addresses stripped) named exactly six rows for each cut, which is what made the layout swings on the other 75 identifiable as noise.

## 2026-11-15 (later): the chain probe in both tiers, and the cell nothing warmed

The three remaining `GetMemberName` buckets (`set_churn` 8.4 M, `map_churn` 6.3 M, `construct_churn` 3.5 M) turned out not to be a layout problem. Two bugs sat in front of the emitter:

- **The optimizing tier's chain probe was keyed with a map id against the IDENTITY table.** `opt_lower::emit_chain_probe` computed `(map_id ^ name) & 15` and read `JitCallContext::member_chain_cells`, which the runtime fills at `(receiver id ^ name) & 15` — so it could only hit by accident, and its entry gate (`array_dense != 0 && named_version == 0`) admitted *only* dense Arrays, i.e. the one receiver the per-step probe declines. Every lifted body's prototype-method read therefore paid the helper: `SLAG_OPT=0` read `literal_proto` at 15.4 ms and the default build at 23.0 ms for the same loop. Porting the per-step probe's identity key into the opt tier (`opcost/proto_method_call` 3.6 M to 112 calls) is the first cut; the two tiers now read one table with one slot arithmetic.
- **Nothing ever warmed the identity cell for a second receiver.** `member_chain_get`'s map-keyed arm (Cut 75) serves a receiver whose own identity cell is cold, and it returned early — so `resolve_chain_cell` never ran, so the identity cell stayed empty, so the machine probe declined forever. Steady state: one helper call per read, for every instance created after the first one. This is why `set_churn`/`map_churn` (a fresh `Set`/`Map` per `bench()` call) and every `function f(){ var o = new Foo(); for(...) o.m(); }` shape never warmed up. The map arm now records the identity cell for the receiver it just validated (the record is exact — `chain_cell_read` validated the links against *this* receiver's live chain and re-checked own-name absence).

Three gates on that record, each measured rather than assumed:

| gate | why |
|---|---|
| `agent.jit_hook.is_some()` | a `--jitless` run has no machine reader; the unconditional record measured `set_churn` +10% and `push_pop` +9% there |
| `named_version == 0` | the compiled probes' generation escape (below); a receiver that owns names needs an unchanged generation, which a receiver with a cold identity cell has not demonstrated |
| not `Array`/`IntegerIndexed` | every compiled probe declines those up front (`array_dense`/`typed_array`), so the record could never be read — `arrays/push_pop` +37%, `opcost/array_push` +43% without this skip |

- **`named_version == 0` is the generation-free shadow proof, in machine form.** The identity probe's receiver-generation check exists to prove no own `name` appeared since the resolution was recorded, but `Set.add`/`Map.set` bump the generation through the internal slot without touching a named property. `named_version` advances on every string-keyed own-property define or delete (a symbol key does not), so `named_version == 0` is the same proof with no dependence on the generation — the Array escape `chain_shadow_free_without_generation` generalized to every ordinary receiver. Both tiers now accept `generation == cell.generation || named_version == 0`.

Same-session, isolated, interleaved, min-of-9..15, against a HEAD binary built the same way:

| row | control | variant | delta |
|---|---:|---:|---:|
| `builtins/set_churn` | 70.5 | 46.4 | -34% |
| `opcost/proto_method_call` | 3.7 | 2.2 | -42% |
| `opcost/map_get` | 4.1 | 3.6 | -14% |
| `builtins/map_churn` | 43.4 | 36.3 | -16% |
| `opcost/map_set` / `set_has` | 3.07 / 3.09 | 2.23 / 2.41 | -27% / -22% |
| `calls/construct_churn` | 68.5 | 72.4 | +6% |

Corpus `GetMemberName` traffic 83.3 M to 52.2 M. `construct_churn` is the one row that pays instead of gaining: its receiver is fresh every iteration, so the opt-tier probe runs and declines without ever hitting — 3.5 M declines, ~1 ns each. Serving it needs the map-keyed compiled probe, which needs the `kind_is_ordinary` mirror the plan's §4 scopes (a Proxy shares the canonical empty map with a plain object, so a map-keyed hit without a kind gate can run a plain object's resolution for a trap). That layout cut stays open, now for one row rather than three.

A measurement trap this arc paid for: **helper counts are not comparable across binaries when the row got faster.** `run_corpus` sizes `reps` from a timing floor, so a faster row runs *more* `bench()` calls and can show a *higher* helper total (`array_at` +9%, `array_includes` +17% while its per-read traffic fell). Read the counts per iteration (divide by the row's `bench` calls) or only for rows whose speed did not move.

### The map-keyed arm, without the layout cut (2026-11-15, later still)

The plan had scoped a `JsObject` layout change for the remaining buckets (a kind mirror plus a properties-length mirror). The kind mirror turned out to be unnecessary — the soundness proof falls out of two behaviours already in the tree — and only the length needed a real field.

**The arm.** After the identity cell declines, both probes now read the MAP-keyed chain cell (`Agent::member_chain_map_cells`, `Vm::member_map_cell_index`) and land on the same link/value tail. Its soundness rests on the receiver owning no `name`, proven structurally instead of scanned:

- the cell is only recorded for a name the receiver's map does NOT describe (new guard in `resolve_chain_cell`: `map.find(&key).is_none()`). Without it a described-but-unwritten hole — absent on the recording instance, so it resolved up the chain — would be recorded, and an instance that did write that field would read the prototype's value over its own;
- so `name` could only be a vector-only own key, which neither admitted shape can hold:
  * `props_deferred` — the vector is empty and every own key is a map descriptor (a boilerplate-presized instance, an object literal, a filled class instance). It also implies **Ordinary**, because `define_fresh` refuses every other kind, which is what makes the kind mirror unnecessary: no exotic (a Proxy most of all) can reach this arm;
  * a dense Array whose vector holds at most the creation's `length` entry and whose `named_version` is still 0 — no string-keyed define or delete has happened since, and an array-length write is in place and advances nothing. A spilled array clears `array_dense` and declines.

**The length mirror.** `properties.len()` was the one unaddressable input (`RefCell`'s payload offset is not guaranteed). It is now a `Props` wrapper — `RefCell<SmallProps>` plus a `Cell<u32>` mirror **outside** the cell — whose `borrow_mut` returns a guard that refreshes the mirror on drop. That makes the discipline structural rather than a rule to remember: a mutation reached through `borrow_mut` cannot forget it. The payload goes 424 → 432 B, which is exactly the old padding (with the 16-byte `GcHeader`, 448 — the same arena class), so the mirror is free and the guard test now pins 432 as the last byte of headroom.

**Results** (isolated, interleaved, min-of-9, both sides built the same way):

| row | control | variant | delta |
|---|---:|---:|---:|
| `opcost/array_at` | 1.9 | 1.1 | -42% |
| `opcost/array_push` | 2.2 | 1.4 | -38% |
| `arrays/push_pop` | 2.9 | 2.0 | -33% |
| `opcost/array_reverse` | 3.0 | 2.1 | -28% |
| `opcost/array_indexof` | 3.0 | 2.2 | -27% |
| `opcost/array_includes` | 3.3 | 2.6 | -22% |
| `calls/construct_churn` | 63.1 | 58.1 | -8% |
| `arrays/typed_array` | 19.4 | 18.3 | -6% |
| `objects/proto_read`, `objects/own_read`, `control/nested_loops` | | | +1.5 / +0.6 / +1.6 (layout noise) |

Corpus `GetMemberName` traffic: 83.3 M at the start of the session, 52.2 M after the identity cut, **27.9 M** now. What is left is `calls/apply_call` 7.0 M (the `Step::CallApply` shape, the banked negative), `opcost/prim_prop` 4.4 M (a primitive string's `.length`, whose native length is not a fixed-offset field), the string-concat pairs ~3.5 M, and `construct_churn` 875 k — the cold first read of each new instance, which is inherent.

**A gate trap.** The first version of the array gate used `named_version == 1` ("only `length` was ever defined"). Arrays measure `named_version == 0`: the creation's `length` define does not advance it, and an array-length write is in place. The `1` declined every array, so the arm looked inert until the census said so.

### Function receivers: `f.apply` in the machine tier (2026-11-15, last)

`calls/apply_call` was the last big `GetMemberName` bucket (7.0 M) and a banked negative: an earlier attempt extended the *identity* probe to function receivers and measured the row 6.5% slower. The reason it did nothing is that the read has to be served at all: `is_plain_object` rejects `TAG_FUNCTION` before the probe looks at a cell, so the read went to the helper both times.

Both chain probes now dispatch on the tag and read the **object part**: a Function value's payload is a `Function` box, and its own properties and `[[Prototype]]` live on `Function::object` — a separate box, which is the identity the interpreter records (`Vm::chain_object` keys function receivers there). The dispatch is a branch that passes the pointer through as a block parameter, *not* a `select` over both pointers: the `select` version cost `proto_read` **30%**, because its speculative load touches another cache line on every ordinary-object read. With the branch, an object receiver's code is unchanged and measures +1.0% (noise).

| row | control | variant | delta |
|---|---:|---:|---:|
| `calls/apply_call` | 41.9 | 26.5 | **-37%** |
| `calls/method_call` | 68.4 | 65.1 | -5% |
| `arrays/hof_methods` | 17.9 | 17.8 | -0.2% |
| `objects/proto_read`, `objects/own_read`, `control/nested_loops` | | | +1.0 / -0.3 / +0.1% |

Corpus `GetMemberName`: 83.3 M at the start of the session, **20.5 M** now. The remainder is `opcost/prim_prop` 7.2 M (a primitive string's `.length`, whose native length is not a fixed-offset field), the string-concat pairs ~3.5 M, `opcost/num_valueof` 1.6 M, and the per-row single digits.

### The canonical-index gate, and why the primitive-read lever was not one (2026-11-15, later)

The handoff said the next member-read cut was a primitive receiver's read: `opcost/prim_prop` 7.2 M and `opcost/num_valueof` 1.6 M `GetMemberName` calls, a string's `.length` (a virtual own property, so the `%String.prototype%` cell can never serve it) and a number's `.valueOf` (no number-prototype arm in either probe). Both are real. Neither is worth a cut, and the measurement is what says so:

- **The interpreter already short-circuits `s.length`** (`get_member_name` step 1), so the helper costs ~2 atom compares and `JsString::len()` past the call. Substituting a dedicated `StringLength` helper would save the generic dispatch, not a chain walk.
- **Sized against a cell-served read, the helper is worth ~12% of the row.** Probe shapes identical except for the read (`scratch/probe/`): own data field served from `member_value_cells` 1.05 ms/100k, primitive-string `.length` via the helper 1.20 ms, prototype-method read via the `%String.prototype%` cell 1.41 ms. A cell or inline-length cut would move `prim_prop` by ~0.15 ms of a 1.2 ms row.
- **The `opcost/*` node column cannot be compared at all.** Those rows are one op over a literal array, so node folds the loop: `array_at` 0.0255 ms for 100k iterations (`s = 300000`), `prim_prop` 0.065, `num_valueof` 0.239 — 18-45x "gaps" that no per-op change reaches and that say nothing about the op. The real rows that node cannot fold are `arrays/index_loop` 4.4x, `for_of_dense` 4.2x, `push_pop` 14x, with identical values.

What the same CLIF read did find is the canonical-index gate, duplicated verbatim at eight sites across both tiers, using the UNSIGNED conversion pair:

```
idx  = fcvt_to_uint_sat.i64 num   ; cvt_float64_to_uint64_sat_seq (multi-branch)
back = fcvt_from_uint.f64 idx     ; u64_to_f64_seq                (multi-branch)
```

x86-64 has no unsigned conversion, so Cranelift expands each into a software sequence — inside the element read's address chain, on every `a[i]`. The signed pair (`cvttsd2si`/`cvtsi2sd`) is one instruction each and exact inside `[0, 2^63)`, and the accepted key set is unchanged (the added `>= 0` bound rejects what `ToUint64Sat` saturated to 0; the round trip still rejects `1.5`, `NaN`, heap bits, and still accepts `-0.0` as 0). The gate and the post-gate conversions are now single-sourced in `crates/jit/src/index_gate.rs` (`canonical_index`, `index_from_number`, `number_from_index`) and every remaining `fcvt_*_uint` on an `i64` is gone from both tiers.

| row | control | variant | delta |
|---|---:|---:|---:|
| `arrays/typed_array` | 19.8 | 17.3 | **-12.6%** |
| `arrays/index_loop` | 15.0 | 14.5 | -3.0% |
| `arrays/push_pop` | 2.10 | 2.07 | -1.8% |
| `arrays/for_of_dense` | 9.60 | 9.69 | +0.9% (noise) |
| `objects/many_objects_read` | 19.44 | 19.35 | -0.5% |
| `objects/warm_store` | 25.27 | 25.34 | +0.3% (noise; +5.8% on a 9-round draw) |

**The next lever, with its evidence.** `arrays/index_loop` (3M iterations of `s += a[i & 199999]`) was 14.5 ms = 4.8 ns/iteration against node's 1.1 ns (4 cycles = one `fadd` latency), with the accumulator round-tripping through the frame every iteration. The cause was *not* missing promotion — `opt/pass/mem2reg.rs` promotes frame slots today — but a missing **resume point**: a fused register body is one `Step::RunRegBody`, a guard needs a step to resume at, and without a guard the element read stays `Type::Unknown`, so `Add` cannot be proven Number and the slot stays heap-possible. Sub-step resume points landed for the computed element read (`register-tier-plan.md` §1.4 has the mechanism and the two traps), and `index_loop` is now 12.2 ms (-16.5%) with `s` a loop-carried register.

### The register body's read guard: sub-step resume (2026-11-16)

The register-tier item the plan had queued as "keep the accumulator in a register" turned out to be a *resume-point* problem, not a promotion one: `mem2reg` already promotes frame slots, but a fused body is one `Step::RunRegBody`, a guard can only resume at a *step*, and without a guard the element read stays `Type::Unknown` (a `+` can be a string concat), so `s` is heap-possible and refused.

`Imm::RegResume { step, op, counter }` closes it for the computed element read. The lowering writes `JitCallContext::reg_resume` (`op + 1`), `run_jit_body`'s deopt arm hands it to `Vm::reg_resume`, and the interpreter's existing call-withdrawal resume pops the mirrored accumulator and continues at that op. Two guards had to be added and one extension refused:

- **Restore `Vm::loop_counter` on the deopt path.** A compiled counter loop keeps `i` in a register and never writes the field; the interpreter's `fast_loop_inc`/`fast_loop_test` *test* the field. Without the restore a mid-loop deopt produced 504 phantom iterations (2004 chars instead of 1500 on `scratch/probe/reg_guard5.js`). The guard's pre-branch flush has already stored the counter's frame slot, so the deopt block copies it into `VM_LOOP_COUNTER_OFFSET`.
- **Require an empty spill stack** — a resume re-enters the step and captures `stack_len` after popping the accumulator, so a live `PushAcc` would leak onto the operand stack.
- **Refused:** the same guard on `GetMemberNameLocal` (`.length`). It regressed every call- and member-heavy fused body (`control/for_in` +32%, `builtins/json_roundtrip` +19%, `arrays/slice_concat` +3.0%) while the narrow rule is a net win. Bodies with a `BinStoreNum` slot also keep the unguarded lowering (`loop_num` is the same class of unrestored scratch).

| row | control | variant | delta |
|---|---:|---:|---:|
| `arrays/index_loop` | 14.56 | 12.16 | **-16.5%** |
| `opcost/element_read` | 0.732 | 0.655 | -10.4% |
| `opcost/array_for_each_js` | 3.934 | 3.746 | -4.8% |
| `control/for_in` | 9.196 | 8.770 | -4.6% |
| `arrays/typed_array` | 18.52 | 17.65 | -4.7% |
| `calls/closure_capture` | 28.89 | 30.07 | +4.1% |
| `objects/many_objects_read` | 19.48 | 20.11 | +3.2% |
| `opcost/dyn_key_read`, `opcost/prim_prop` | | | +0.9 / -1.5% |

`arrays/index_loop`'s loop body is now `addsd` on a loop-carried xmm plus one flush per guard, instead of a load-add-store round trip. Correctness net: `a_fused_body_read_guard_deopts_and_resumes_the_program` asserts parity *and* a deopt on a loop whose element turns into a string at i == 500 — the result's `calls` counter is what proves the resume position, since an unguarded lowering computes the same value.

### The inline pop (2026-11-15, last)

`arrays/push_pop` was the largest real row-vs-node ratio in the corpus (14x) and the census explained it in one line: `ArrayPush` 120 calls, `ArrayPop` **1,439,232** — the append has had an inline machine path for a while, pop was a helper call per operation. `emit_array_pop_inline` in both tiers shears the tail when `array_dense` is live, `is_prototype == 0`, `length == elem_len` (a tail of holes is spec-absent), `elem_len != 0` and the element is not the hole sentinel; anything else keeps the helper. It skips the write barrier (removal only), the [[Extensible]] check (`length` is an existing own data property), the `properties[0]` mirror write (not offset-addressable — the append skips it too, and the cell is authoritative while dense) and the holed vacated slot (the cursor is the geometry, so `[elem_len, cap)` is neither traced nor readable).

| row | control | variant | delta |
|---|---:|---:|---:|
| `arrays/push_pop` | 2.554 | 1.703 | **-33.3%** |
| `opcost/array_push` | 1.607 | 1.495 | -7.0% (layout) |
| `arrays/index_loop` / `arrays/hof_methods` | | | +5.5 / +5.3% (layout) |

Those two contain no `pop` and are pure layout: `ir_diff.py` reports **0 of 81 rows** with different IR, which is the tell for a lowering-only change (the tier's IR is untouched; only the emitted code moves). For that class the helper census is the discriminator, and it is exact here — `ArrayPop` 1.44 M -> **0** with the row's value unchanged.

## 2026-11-17: a per-iteration closure was never a leaf (the empty `per_iteration_chain`)

`head-let`'s 3-per-iteration closure call was the last big measured lane item
(`call-lanes.md` §26). The attribution was wrong in the doc: it is not the call
lane, it is the **closure's own body**, and it had never been compiled as a
leaf at all.

`register_function` compiled the body (`shared_compiled_body`) and installed
`data.per_iteration_chain` **after** that compile, so the first (and only)
compilation of a closure body created inside `for (let i ...)` saw an empty
chain. `i` therefore lowered to `Step::LoadIdent` — a dynamic environment walk
per read — instead of `Step::LoadPerIteration`. `LoadIdent` is not a leaf step,
so the body was never leaf-certified, and every call fell to the certified
helper (`CertifiedCall` 7.0 M on a 1 M-iteration probe). Two further bugs
surfaced once the chain was actually used: `closure_per_iteration_chain`
recorded the *outer* enumeration position as the env hop, so a loop nested in
another loop read the wrong per-iteration env (`total - 1 - position`, matching
`per_iteration`); and a generator/async body must record no chain at all — its
per-iteration envs only exist while it is suspended, so a closure created there
resolves through the env walk (`resumable`).

With the body a leaf again, the call site needed the body-site keyed record that
was already in flight, plus:

- the env lane loads the **live** callee's [[Environment]] from the `Function`
  box (`Function::ENVIRONMENT_OFFSET`; the edge is now one traced word, not an
  16-byte `Option<GcAny>`), because a per-iteration closure is a new box each
  call and the record's copy belongs to an earlier one;
- the probe may record an `uses_env` leaf while a per-iteration env is open
  (the in-frame lane still requires the body's own env only);
- the stale-epoch route (`cert_ready`) offers the env lane before the certified
  helper, and a body-sited hit may use either lane — the frame descriptor is
  body-static, and the environment is the one fact taken from the live callee.

The sweep caught a real regression in the in-flight descriptor cache: serving
the record's certified verdict to `call_slow` while a `try` frame is open let a
throwing callee's error escape the caller's `catch` (test262
`language/statements/class/subclass/builtin-objects/TypedArray/super-must-be-called.js`).
That route now requires the same "Vm stacks clear" gate the compiled site uses
(`emit_leaf_cert_ready`), and derives the verdict otherwise.

| probe (min of 5 interleaved pairs) | control | variant | delta |
|---|---:|---:|---:|
| per-iteration capturing closure call, 1 M iterations | 657.6 | 379.6 | **−42.3%** |
| the same shape at 3 calls per head-let iteration (100 k) | 233.5 | 178.8 | **−23.4%** |
| fresh non-capturing closure call, 1 M | 371.2 | 298.4 | −19.6% |
| fresh non-capturing, 3 per iteration (100 k) | 113.7 | 106.7 | −6.2% |
| one closure per body, called 3× | 47.1 | 39.1 | −17.1% |
| one closure per body capturing an outer `var` | 53.1 | 41.5 | −21.9% |
| closure creation only | 298.3 | 303.1 | +1.6% (noise) |
| stable leaf call in a loop | 8.94 | 8.83 | −1.3% |

`CertifiedCall` on the 1 M probe drops **7,000,000 → 2,287**; what is left of
the row's ~380 ns per iteration is closure creation (~300 ns, a separate
lever) plus the loop's own per-iteration env bookkeeping. The call lane is no
longer the dominant term.

Gates: `sweep all` 48,633/48,633 (0 fail, 0 skip, 0 hang), `cargo test
--workspace` green, clippy `-D warnings` clean.

## 2026-11-17: the other half of the closure cost was the site's static facts

With the call lane fixed, the per-iteration closure row was ~380 ns per
iteration of which ~300 ns was **creation** — and creation re-derived, per
closure, four things that cannot differ between closures of one site: the
shared body, params and source texts (three thread-local `HashMap` probes), the
body-directive strictness (an AST scan), and the compiled body (a per-agent
`compiled_bodies` probe). None of them is large; together they are most of the
row. A 1 M-creation probe with the nursery threshold swept from 256 to 65,536
boxes moves by nothing (309 / 293 / 312 ms), so the cost is the work, not the
collector.

`SiteTemplate`/`SiteCell` (`function.rs`), carried by `Step::CreateFunction`,
now hold those five facts; the site's first closure resolves and fills them and
the rest read them. The template lives in the agent's compiled body, so its
`Rc`s belong to the same agent as the closures. Arrows got one more cut on the
same pass: they materialized `length`/`name` eagerly while a plain function
deferred them to `ensure_function_properties`, so an arrow creation paid the
shape fork. Deferring it needed `ensure_function_properties` to derive
AddRestrictedFunctionProperties from `[[ThisMode]]` instead of strictness — a
lexical-this arrow must never get sloppy `caller`/`arguments`.

| probe (min of 5–7 interleaved pairs) | before | after | delta |
|---|---:|---:|---:|
| closure creation only, 1 M | 296.3 | 214.3 | **−27.7%** |
| arrow creation only, 1 M | 330.3 | 271.5 | **−17.8%** |
| per-iteration capturing closure + call, 1 M | 374.1 | 315.3 | −15.7% |
| the head-let shape (3 per iteration, 100 k) | 191.4 | 160.3 | −16.3% |

Cumulative against the pre-body-site control over both cuts: the per-iteration
capturing closure 643.9 → 313.4 ms (**−51.3%**), the head-let shape 247.0 →
158.8 (**−35.7%**), a stable closure −17%; the untouched stable-leaf row is
flat. What remains of the creation row is the `ecma_functions` insert (one
record per live function), the arena allocation and the per-closure context
reads.

Arrows then take the same template (`Step::CreateArrow` carries its own
`SiteCell`), which is where the rest of their creation cost was: **−34.6%** on a
1 M-arrow-creation probe and −20.3% on the per-iteration capturing arrow
against the previous commit (−31.3% / −38.4% against the pre-body-site
control), while the function-expression rows stay flat (c0 −1.2% at nine
interleaved pairs).

Gates: `sweep all` 48,633/48,633, `cargo test --workspace` green, clippy
`-D warnings` clean.

### Where the rest of the creation cost is: not the record (and the phase instrument cannot say)

`SLAG_CLOSURE_PHASES` reports `function_new` 299, `register` 155, `record` 92
per closure on the creation row. Those numbers do not reconcile with the wall
clock: the same row measures 239 ns per closure isolated, while the phase values
sum to ~430 after subtracting the instrument's own empty-block line (39 ns) — the
instrument's `callee_total`/`call_register` lines imply a 4–5x inflation. That is
`slag-object-layout` §5's effect: an `rdtsc` bracket cannot be overlapped by the
work between brackets, so a small phase reads far above its marginal cost, and
the empty-block calibration only measures the bracket (it has no work to
serialize). **Treat no sub-100 ns phase split from this instrument as
attribution.**

What is solid:

- `size_of::<JsObject>() = 432`, `size_of::<Function>() = 600`. That is ~10x a
  V8 object header, and it is the one number here that is not a measurement
  artefact.
- Isolated `crux` micros, 1 M *live* boxes: a bare 432-byte arena payload 110 ns,
  `ordinary_object_create` 132–140 ns, `Function::new_with_prototype` 203 ns,
  `canonical_empty_map`'s cached path 12–18 ns. These are **footprint-inflated**
  (432–600 MB of first-touch pages); `slag-object-layout` §5 records the
  recycled steady state as ~14 ns for `ordinary_object_create`, and a nursery
  sweep on the real row (256 → 262,144) moves nothing, which rules the same
  footprint effect out there too.
- So the ~239 ns is a long tail — the box, the `EcmaFunction` literal, the
  `ecma_functions` insert, four context reads, `canonical_empty_map`, `name`, and
  the return — with no single item large enough to justify the
  `Rc<SiteFacts>` refactor that was queued next (the insert writes into an
  existing hashbrown bucket array; there is no per-entry allocation to remove).
  **Not taken**, and a real profiler — not this instrument — is the prerequisite
  for the next attempt.

Isolated real rows (one workload per process) against this session's start and
against node v24:

| row | before | now | node | gap |
|---|---:|---:|---:|---:|
| `for-in/head-let-fresh-binding-per-iteration` | 511.6 | 460.7 | 62.0 | 7.4x |
| `calls/closure_capture` | 44.3 | 33.9 | 2.36 | 14.4x |

Both rows are call-heavy with stable callees, and node's advantage on them is
inlining, not allocation: `closure_capture` calls one of 64 closures through an
array index a million times, which V8 inlines away. The whole-corpus pass
(`bench.js`, 81 rows in one process per mode) reads mean-jitGap 12.85 overall
with result parity across all four modes.

## 2026-11-17: the optimizing tier was not running on the two biggest rows

A helper-call *signature* whose parameter count disagrees with the call is not a
refusal. Cranelift's verifier rejects it at assembly, `opt_lower::compile`
answers `None`, and the body silently drops to the per-step tier while its IR
dump still looks perfect. Four sites had a signature one parameter too wide
(`PerIterEnter`/`PerIterNext`, `PerIterLoad`, `PerIterStore`, `StringSub`), so
every body containing a per-iteration environment failed to assemble — which is
`head-let` and `evaluation-order`, 47% of the corpus's total gap.

Found with `JIT_DUMP_CLIF=1` (`jit codegen error: … mismatched argument count`
then `opt assemble failed`), attributed by running one row per process and
grepping for `codegen error`: exactly 2 of 81 rows, the two largest. `call_helper`
now debug-asserts `signature.params.len() == args.len() + 1`; that assertion
immediately caught the fourth site (`Op::StringSub`, which only
`installed_jit_template_rope_matches_the_interpreter` builds).

| row | control | variant | delta |
|---|---:|---:|---:|
| `head-let` (min of 9 interleaved pairs) | 487.1 | 456.2 | **−6.3%** |
| `evaluation-order` (min of 7) | 249.3 | 241.1 | −3.3% (medians flat) |

`evaluation-order` barely moves, and that is the useful part of the result: its
body enters the tier now, but its cost is template strings, per-body closure
creation and calls, none of which the tier lowers differently from the per-step
path. Being in the tier is a precondition, not a win.

**The second cut is a missing fast path, not a missing port.**
`Object.values`/`Object.entries` read each key's descriptor (to test
enumerability), then called the full `get_property` — a proxy check, a chain
walk and a getter dispatch — to read a value the descriptor already held. For an
own **data** property of an Ordinary object or an Array, `Get` cannot run user
code and cannot consult the chain, so the descriptor's value *is* the answer.
Accessors, exotic receivers (module namespace, Proxy, mapped `arguments`,
boxed strings, typed arrays) keep the general path; a function's deferred
boilerplate cannot be observed here, because every structural operation that
adds an enumerable own key materializes it first.

200k-iteration probe (`scratch/probe/ok/values_200k.js`), six interleaved pairs,
control built from the tree without the cut: `Object.values` 155.5 → 105.9 ms
(min), **every pair separated** — −32%. `Object.entries`
(`entries_200k.js`) is unchanged within noise, because its cost is the per-pair
array allocation (`array_create` + two element defines ≈ 230 ns per pair, against
a JS `[k, v]` literal's ~150 ns). The corpus row that exercises all three
(`builtins/object_keys`) reads 83.6 → 70.1 ms in a whole-corpus pass.

**The third cut is the same allocation, one layer down.** `array_from_values`
(CreateArrayFromList, the funnel every array-returning builtin uses) built its
array with one `create_data_property_index` per element. That is the dense
store's *slow* shape: a write barrier, an `elements_mut` borrow with a possible
buffer grow, a `length` mirror write and a generation bump **per element**
(~55 ns each), where `JsObject::array_extend_dense` — already the array
literal's fill (`slag-dense-arrays` §9) — does one borrow, one grow and one
length/generation update. The create must stay *empty*: `array_extend_dense`
appends at `length`, so pre-sizing to `values.len()` would pad holes first.

| probe | control | variant | delta |
|---|---:|---:|---:|
| `Object.keys` of a 100-key object (4 pairs) | 4.35 | 1.86 | **−57%** |
| `builtins/object_keys` (9 pairs, minima) | 45.7 | 37.8 | **−17%** (medians −8.6%) |

The 100-key probe is the one that reads the mechanism: the per-element term is a
*fixed* cost per element, so it is invisible on a 3-element array and dominant on
a hundred. A 6-element corpus row reads −8 to −17% with 7 of 9 pairs separated;
`strings/split_join` was inconclusive in this window (its arrays are small and
the host's spread on that row was ±25%), so it is not claimed either way. The
whole-corpus total is not quoted for this cut: three passes of the same tree in
this window read 2211, 2303 and 2429 ms against node's 913, 928 and 935, which is
host drift, not the change.

Whole corpus, same session, 5 interleaved rounds: **2511.5 → 2211.4 ms** across
the arc, with node 953.8 → 912.6 — the ratio reads 2.63× at the start and
**2.42×** at the end. Both engines drift with the host, so take the row-level
numbers and the disappearance of `opt assemble failed` as the evidence and the
ratio as the direction.

The census that follows from this arc is the *allocation* line: a 2-element
array literal costs ~150 ns, an empty array that escapes ~83 ns, and a GC box
~44 ns — one `JsObject` plus one `ArraySlots` box is the whole difference, and
that is what `Object.entries` and every string-building row pay per element.
Node's equivalents are 5-10 ns, so this is where the next structural cut is, not
in another register-tier lift.

The same session scoped the other half of that line — the call lane — and landed
neither: `slag-jit`'s `call-lanes.md` §26 has the census and the design. In
short, a closure created and called inside a loop (three of them per `head-let`
iteration) is keyed by its box identity, so it never hits the call-site record:
it pays a probe plus `call_slow` per call, ~650 ns against ~15 ns for the same
call through a stable callee. Serving it needs the record keyed by a *body site*
instead, and two facts make that sound (`globals_unshadowed` is site-invariant;
a `uses_env` leaf additionally needs its environment compared).
Two smaller experiments were measured and reverted rather than landed: reusing
the probe's certified verdict from `call_slow` (−10% on the capturing probe, flat
on `head-let`) and building an `Object.entries` pair through `array_from_values`
(neutral at two elements).

## What the current design has taken as far as it goes

The register and materialization work is exhausted, and it is exhausted at
parity: the lifts that admit more bodies to the optimizing tier, the
per-iteration environment family, `is()`, `!==`, measured 1.002–1.020x against
`SLAG_OPT=0`. That is coverage, not speed, and it is worth having, but it is not
a lever.

The remaining gap is structural. Function-object creation costs ~340 ns per
closure where node eliminates a non-escaping closure outright (0.6 ns per
iteration), and the two `language` fixtures, which create closures per
iteration inside a `for (let x in obj)` head, are ~45% of the corpus's total
gap. No amount of register traffic reaches that.

## What is next

`.notes/register-tier-plan.md` is the plan of record, and its §5 ordering is
not the phase list read top to bottom: **A0**, function-object creation itself,
the ~340 ns term, gates the corpus total, because the two `language` fixtures
are ~45% of the gap and the closure path is two thirds of the larger one. A1/A1b
(coverage) is a precondition on those rows but is not what has to move, **D5**
(the callback lane) must precede **E** (generalized escape analysis) because
escape analysis has nothing to work on until a call boundary disappears,
**F1/F2** (the effect oracle, then GVN/loads) follow, and **B** (the typed value)
is off the critical path, the §1.3 census says the materialization residue is
small. C2 (a stack map) is owed by the first cut that keeps a heap reference in
a register. §1 has the census and the ablation, §7 the traps.

## Gates

Every performance change carries its own documentation in the same commit, and
holds all of:

- the corpus differential exact, `mismatches=0` across `jit`, `jitless`, node,
  and node `--jitless`, with identical results under `--gc-stress` and
  `--nursery-stress`;
- test262 `language`, `built-ins`, and `annexB` at 0 fail in both modes;
- workspace tests, `cargo clippy --workspace --all-targets -- -D warnings`, and
  `cargo fmt --all -- --check`.

A refusal is a parity decision, not a gap: the tier declines a step kind rather
than miscompiling it, and `JIT_DUMP_IR` names every refusal. A lift that makes a
row slower by more than 2% is a missing parity port, not a result.

## Measuring

`.agents/skills/slag-perf-measurement` is the protocol and the trap list: same
session, interleaved, min-of-N; never compare across windows or across binaries
built from different trees; check binary freshness before trusting a number.

Instruments: `node tools/corpus/bench.js` (the cross-engine run that prints the
table above), `python tools/corpus/measure.py` (interleaved min-per-row totals),
`ab_lift.py`, `census_probe.py`, `refusal_probe.py` (now including the
`opt-lower bail` gate, which is a different refusal from `opt bail` and hid six
rows), `ir_diff.py` (which rows' IR two binaries actually differ on, so a
delta on any other row is known to be layout), `helper_rows.py` (the
`JIT_HELPER_STATS` histogram attributed per row, one row per process) and
`seam_ab.py` (a same-binary A/B of an env-gated pass, the only protocol that
resolves a row under ~5 ms); in the engine, `JIT_DUMP_IR`,
`JIT_DUMP_CLIF`, `JIT_HELPER_STATS`, and `--features step-stats`.
`ENVIRONMENT.md` documents each switch.
