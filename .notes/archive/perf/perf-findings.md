# Performance findings: the wider map, and the numeric hole

Measured 2026-10-03 with `tools/corpus/sm-ab.sh` against the SpiderMonkey js shell
(`JavaScript-C159.0a1`), release `slag`, min-of-3, result parity checked on every
row. `opcost` is the synthetic single-op family; the seven others are the
realistic workloads. The headline is a gap the optimizing-tier plan does not
cover, because it is not about specialization at all.

## 1. The wide map

| family | worst rows (gap, this-run) | reading |
|---|---|---|
| objects | `destructure` 112×, `spread_assign` 40×, `warm_store` 25×, `compound_assign` 20×, `many_objects_read` 12×, `proto_read` 9× | stores, spread and escape analysis; `own_read` 2.1× but `proto_read` 9× |
| arrays | `push_pop` 54×, `typed_array` 51×, `slice_concat` 19×, `hof_methods` 17×, `index_loop` 7.7× | typed-array read+write is 33 ns/iter; callback inlining |
| strings | `char_ops` 82×, `search_slice` 38×, `split_join` 13×, `concat_loop` 12× | char intrinsics; rope/concat |
| builtins | `math_intrinsics` 35×, `object_keys` 29×, `map_churn` 21×, `set_churn` 18× | the stage-B case, confirmed |
| control | `generator_loop` 42×, `try_catch_loop` 23×, `nested_loops` 10× | generator resume; try machinery per iteration |
| calls | `closure_capture` 21×, `method_call` 14×, `recursive_fib` 9.4×, `direct_leaf` 2.2× | resolution and dispatch, not the call itself |
| globals | 1.5–4.3× | the one family already given focused work |

The strategic read: where the engine has already done targeted work (`globals`,
`for_of_dense` at 1.2×) the gap is small; every untouched surface is 10–100×.
The deficit is real but **per-surface** — the mechanism is uniform, the work is a
portfolio of slices, which is what O/B/I/E/L/T builds.

## 2. The numeric hole

`nested_loops` is pure integer arithmetic — no calls, no allocation, no
properties — and it is 10× off. Probe (1M iterations unless noted; ns/iter):

| body | slag jit | SM | note |
|---|---|---|---|
| `s = s + i` (double) | **0.78** | 0.63 | at parity |
| `s = i` (int store) | 0.90 | 0.35 | fine |
| `s = i + 0.5` (num store) | 0.86 | 0.42 | fine |
| `s = (i + 0.5) \| 0` (coercion, no carried read) | 1.93 | 0.52 | fine |
| `s = (s + 1) \| 0` (carried, truncated) | **7.21** | 0.38 | 19× |
| `s = (s + i) \| 0` | **7.08** | 0.30 | 24× |
| `s = (s + i) & 1073741823` | **7.24** | 0.42 | 17× |
| `s = (s + i) \| 0; s = s \| 0` | **12.3** | 0.28 | scales per coercion |
| `s = s + ((i*31+5) % 7)` | **8.80** | 1.31 | see `%` below |
| `nested_loops` shape (4M) | **11.2** | 1.05 | the workload |

So the coercion in isolation is ~1.9 ns and an int store in isolation ~0.9 ns,
but an int32 **loop-carried accumulator** costs ~7 ns/iter, 9× its own double
equivalent and ~20× SM. It is not the coercion and not the store — it is the
carried slot's representation.

### Root cause 1: the fast loop lane is f64-only

The register executor rewrites a plain `slot = slot <arith> rhs` loop into
`LeafOp::BinStoreNum`, which runs raw f64 on `Vm::loop_num` with no tag checks
and no guards (`crates/runtime/src/ir.rs` `plan_loop_num` :18098, `rhs_recipe`
:22063, `run_leaf_ops` :11553; JIT side `emit_num_slot_rmw`
`crates/jit/src/compiler.rs` :8813). That is why `s = s + i` is 0.78 ns.

The lane admits only `is_inline_arith` store ops (Add/Sub/Mul/Div, `ir.rs`
:22021) and a Number seed, and every `NumRhs` variant (`ir.rs` :1217) is
floating-point. `| 0` / `& mask` / a shift **wraps** the RMW in a bitwise op, so
the shape does not match and the loop never enters the lane.

It is *not* that the operator is missing, which is what this note first said:
G9 (`jit-quality-plan.md` §3) already inlined the six integer operators as a
guarded i32 op (`emit_int_binary`, `compiler.rs` :8657). What remains is that
the inline is not raw — every execution runs the per-op range guard (`trunc_i32`:
`fabs` + `fcmp` + a slow-block branch, :8698) and round-trips the accumulator
through `Value` bits, because the loop is on the generic step/leaf path instead
of the register lane. That round-trip is the 7 ns.

**Landed (A1, 2026-10-03).** `plan_loop_int` recognizes `slot = (slot <arith> rhs)
| 0` / `& mask` (the 3-/4-op register shapes the dump showed:
`[BinImmLocal{op,slot,imm} | LoadCounter/LoadConst + BinLeftReg{op,slot}, BinImm{BitOr,0}|BinImm{BitAnd,m}, StoreReg{slot}]`)
and runs it as wrapping i32 in the **shared** `Vm::loop_num` / JIT num-slot
register (an int32-valued f64), so no new `Vm` field, register, or bind/store path
was needed; the trailing truncation folds into an AND mask (`|0` = `-1`). Sound
only for a rhs the plan proves int32 (an int32 immediate, or the counter of an
ascending loop to an int32-immediate limit from an int32 seed). Measured (1M iters, release): `s=(s+i)|0` 7.08 → **0.74** ns, `s=(s+1)|0` 7.21 → **0.82**
ns, `s=(s+i)&mask` 7.24 → **0.83** ns — **parity with the f64 Number lane (0.78
ns)**, ~2.4× SM's ~0.30 ns. The first cut (A1.0) kept the accumulator in the
shared `Vm::loop_num` / f64 register and only reached 2.75 ns; the finish (A1.1)
moved it to a dedicated **i32 register** (the JIT detects the int slots from the
body's `BinStoreInt` ops, so no `Step`/`Vm` field was needed) and dropped the
per-iteration f64↔i32 round trip — 2.75 → 0.74. Values match the interpreter,
corpus parity holds (36/36), 5566 workspace tests pass.

**Why the proof is required (and what A2 adds).** The wrapping op equals `ToInt32`
only when `f64(slot <op> rhs)` is exact: `ToInt32(s + x) == wrap32(s + trunc(x))`
holds for integer `s`, but JS adds in f64 *first*, so the lane diverges whenever
`s + x` rounds (`x = 2^60`, `s = 1`: JS `|0` gives 0, an i64 add gives 1). The f64
lane gets this for free (a Number has no truncation); the int lane needs an int32
proof. A1 gets it at plan time and omits the guard; a rhs it cannot prove (a slot,
or an unbounded counter) still takes the step path. A2 covers those with a
per-iteration exactness guard plus a bail (the JIT has `DISPATCH_DEOPT`; the
register executor does not) on the same `BinStoreInt` op — which is why A1 was
built with an `IntRhs` enum rather than a hardcoded immediate.

### Root cause 2: `%` is not inlined, by design

`BinaryOp::Mod` is absent from `inline_binary` (`compiler.rs` :521) and from
`is_inline_arith` / `num_arith` (`ir.rs` :22021 / :22045), so every `%` routes to
`BinarySlow`: 8.8 ns vs 1.31 for SM.

This is deliberate, not an oversight: cranelift 0.134 has no `frem`/`fpow`, and a
formula reimplementation would not match Rust's `%` bit for bit (`compiler.rs`
:475). So the fix is not an `frem` arm. Two options, in order of effort:
(a) a direct, untagged f64 remainder helper — on the register lane both operands
are already known Numbers, so this skips `apply_binary`'s dispatch and tag
checks, ~8 → ~3 ns; (b) to reach SM's ~1 ns, an integer `srem` behind an
integrality + int32-range + `rhs != 0` guard.

Both are **local fixes**, not new subsystems, and neither is covered by stages
O/B/I/E/L/T (which are all about specialization). Slag proves the machinery can
be fast — the double path is at parity — so this is a hole in the baseline
numeric lowering, and it is the largest single term in the map.

## 3. Call-site findings

- **Method-call resolution.** `method_call` is **57 ns/call** while `direct_leaf`
  is **6.5 ns/call** — 9× apart; on SM the same pair is 4.1 vs 2.9 ns (1.4×). So
  the cost is resolving `counter.inc` through the member machinery, not the call.
  A monomorphic method-call IC that caches the resolved callee at the site closes
  most of it, independent of inlining.
- **Computed / megamorphic callee.** `closure_capture` (`fns[i & 63](i)`,
  64 distinct callees) is **46.7 ns/call** vs SM 2.2 ns. The computed-callee
  dispatch is very slow even before it is megamorphic.

## 4. Other per-surface levers

| surface | evidence | lever |
|---|---|---|
| prototype-chain reads | `proto_read` 20.5 ns/read vs SM 2.4 (`own_read` is only 2.1×) | a chain-caching IC that keys the lookup on the holder's shape |
| warm stores | `warm_store` 14.9 ns/iter vs SM 0.61 | inline the shape-check + slot write (the write counterpart of the read LICM) |
| typed arrays | `typed_array` 33 ns/iter vs SM 0.65 | inline TA element read+write; the read path exists, the store is the gap |
| generator resume | 502 ns/`next()` vs SM 12 | the suspension driver (a separate workstream; not in the tier plan) |
| strings | `char_ops` 82×, `concat_loop` 12× | char-code intrinsics; rope building on concat |
| try/catch in a loop | `try_catch_loop` 23× | the per-iteration try machinery |

## 5. Ranking

1. **Int32 register lane** (`s = (s + x) | 0`) — **A1 landed 2026-10-03**
   (7.1 → 2.75 ns, shared `loop_num` register, proof-based rhs). Remaining: the
   A2 guarded rhs (a slot / unbounded counter), and an i32-register accumulator
   to drop the f64↔i32 round trip.
2. **`%` via a direct f64 helper** — 6.7× on the modulo rows, and a mechanical
   helper-mirror job; the integer `srem` fast path is a later refinement.
3. **Builtin intrinsic inlining (stage B)** — widest row count (10–72×).
4. **Method-call IC + computed-callee dispatch** — 9–21× on the call families.
5. **Escape analysis (stage E)** — the biggest ratios (`destructure` 112×).
6. **Store inlining, proto-chain read, typed-array store** — 9–25× each, one
   surface at a time.
7. **Generator resume** — 42×, but its own subsystem.
