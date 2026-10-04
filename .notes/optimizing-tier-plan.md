# The optimizing tier: specialize and retire, never deopt

SpiderMonkey is the primary source. The organizing principle is that Slag does **not** build a deoptimizer: it compiles specialized bodies under premises the engine already validates, and when a premise dies the body is **retired** and execution continues in the interpreter. The compiled frame *is* the interpreter frame, so there is nothing to reconstruct. This is the one thing V8, SpiderMonkey and JavaScriptCore cannot do — their optimized frame is not their baseline frame — and it is what makes the rest of the machinery smaller for us than for them.

`optimizing-tier-impl.md` is the engineering companion: where the code goes and what proves each step. This document is the design and the strategy.

## 1. Why this exists

The inline-coverage arc (`.notes/jit-quality-plan.md`) did what it set out to do: the helper-share census named the hot surfaces (G1–G21) and each landed slice moved its row. But four of those slices moved individual corpus rows 17–46% and **moved nothing else** (`jit-quality-plan.md` §6), because the design they work inside is unchanged: compiled dispatch with inline fast paths, no type feedback, and no speculation. Every non-trivial `Step` either inlines a narrow case or calls one of the helpers.

The 2026-10-03 re-baseline against the SpiderMonkey js shell (`tools/corpus/sm-ab.sh`, 36 `opcost` rows, all parity `ok`) says the deficit is not "our machinery is slow." SM is ahead on **every** row, from `js_call` 3.6x to `object_keys` 234x, and the pattern is uniform: SM inlines the builtin and elides the allocation; we emit a generic call and allocate.

| row | gap | what SM does |
|---|---|---|
| `object_keys` | 234x | knows the answer; elides the keys array |
| `array_at` | 139x | folds the whole operation on constant input |
| `math_abs` | 72x | lowers `Math.abs` to a machine op |
| `string_charat` | 65x | inline element load |
| `typed_array_for_each` | 46x | inlines the callback, elides the iteration scaffold |
| `array_slice` | 33x | escape analysis removes the result array |
| `regexp_test` | 31x | inline fast path |
| `set_has` / `map_get` | 29x / 28x | inline hash probe |
| `array_alloc` / `object_alloc` | 20x | escape analysis removes the allocation |
| `method_call` / `js_call` | 6x / 3.6x | inlines the callee |

### It is not folding — it is specialization

The obvious hypothesis, that SM merely constant-folds the toy rows, is wrong, and the probe that shows it is the point of this section. Const/varying pairs:

| row | SM | Slag | gap |
|---|---|---|---|
| `small.at(2)` const | 0.054 ms | 7.95 ms | 147x |
| `small.at(k & 3)` varying | 7.49 ms | 7.95 ms | **1.06x** |
| `a.indexOf(3)` const | 0.85 ms | 8.68 ms | 10x |
| `a.indexOf(k & 3)` varying | 0.86 ms | 8.12 ms | **9.4x** |
| `a.slice(1,3).length` const | 0.98 ms | 29.6 ms | 30x |
| `a.slice(k & 3,3).length` varying | 0.90 ms | 27.9 ms | **31x** |

`indexOf` and `slice` pay the *same* gap whether the operand is constant or not — so the win is not the fold, it is that SM specializes the operation. `Array.prototype.at` is the lone 1.06x because SM leaves it as a generic call, exactly like ours. The gap is **specialization machinery**, uniformly.

### Front end, not back end

This is the reason the plan is ordered the way it is. Cranelift's generated code is, by its own account, roughly 2–3x slower than LLVM. Our gaps are 10–230x. So at most ~2–3x of any row is the backend and **90–95% is the front end**.

It is starker than the ratio: `Math.abs(k)` costs Slag 28.5 ns/op. A `fabs` is sub-nanosecond, and a poor one is 1–2 ns — **none** of that 28 ns is codegen, it is the call and the boxing. `({a:1}).a` is an allocation and a lookup that SM folds to `1`. No backend recovers a call we should not have emitted or an allocation that should not exist. The 2–3x is the *last* thing, and the lever there is a top tier (SM's FTL adds LLVM only for the hottest functions), not a backend swap — LLVM's compile time is precisely why V8, SM and JSC all wrote their own backends.

## 2. Our way: specialize, then retire

Every engine's optimizer is built on **speculate → guard → deopt**, and deopt is the expensive half: V8's translation opcode stream (`deoptimizer/translation-opcode.h`), SM's resume points and `Recover_*` (`Snapshots.h`), JSC's `ExitTimeObjectMaterialization`. All of it exists for one reason: their optimized frame is not their baseline frame, so the optimizer's eliminated state must be *reconstructed*.

Ours does not have to be. The compiled body already addresses the same `vm.stack` the interpreter does — `DISPATCH_DEOPT` assumes exactly that today, and `call-frame-plan.md` makes it literally one frame. So we replace deopt with **retirement**:

- **Premise, not guess.** A specialized body is compiled under premises that are *checked where the engine already checks them*: callee identity through the property cell, receiver shape through the member/element cells, builtin semantics through the builtin registry, non-escape as a static property of a wholly-compiled body. These are not speculative type feedback; they are facts the engine maintains.
- **Retire, don't reconstruct.** When a premise is contradicted, the body is retired (the same generation/epoch invalidation the cells already perform) and the next entry recompiles or falls back to the interpreter. An activation already running completes on the old code (single-threaded; the contradiction can only arise from a nested call, which happens at a step boundary anyway).
- **Exit, don't deopt.** Where a runtime check is cheaper than coarse invalidation (a polymorphic shape), a failed check exits to the interpreter via the existing `DISPATCH_DEOPT` path. That path spills the working region into `vm.stack` and resumes at `vm.ip` — a *resume*, not a reconstruction, because the frame is already the interpreter's.

The invariant that keeps this true, and the plan's central constraint:

> **Never let the compiled frame diverge from the interpreter frame.**

The day inlining or escape analysis makes them diverge is the day we owe SM's snapshot machinery. The design keeps them identical, so we never pay for it.

**Honest risk.** Retire-and-recompile can thrash a hot polymorphic site where lazy deopt would not, and a recompile has latency. That is not assumed away: the first stage measures it, and the megamorphic valve (§3.2) is the fallback. The bet is that whole-body recompilation-on-invalidation is affordable *because* Slag compiles one body at a time and the invalidation substrate already exists.

## 3. What SpiderMonkey teaches

Read from the tree (`js/src/jit/*`, `js/src/doc/*`, the `[SMDOC]` comments). Full notes in `spidermonkey-study.md` and `spidermonkey-wasm.md`. SM is the better model for us than V8 in four places, and it deletes.

### 3.1 Feedback is a program, not a hint (CacheIR)

CacheIR (`CacheIR.h`, `[SMDOC] CacheIR`) is a per-site *guard+op data IR* — `GuardToObject`, `GuardShape`, `LoadFixedSlotResult` — whose operands are small typed IDs and whose references are stub-data offsets, so many sites share one compiled stub. `WarpCacheIRTranspiler` lowers it to MIR *mechanically* (`emitGuardShape` is about five lines). V8's `FeedbackVector` is closer to a bag of hints the optimizer re-derives; SM makes the feedback a program the optimizer **transpiles**.

This is the single most important borrow. We already have the two hard halves — one shared code representation (the `Step` stream) and an invalidation substrate (the member/global/leaf cells and their generations) — and we lack only the per-site record *as data*. SM proves that the record should be data, shared by both tiers, and mechanically consumable.

### 3.2 `ICState`: the polymorphism valve, already designed

`js/src/jit/ICState.h` is small and worth copying outright. Per site: `Mode { Specialized → Megamorphic → Generic }`, `MaxOptimizedStubs = 6`, and an *adaptive* failure budget `maxFailures() = 5 + 40 * numOptimizedStubs` (a site that has attached stubs gets many more attempts). This is precisely the valve that stops the retire mechanism thrashing: past the budget, the site stops specializing and the body runs at the (competitive) baseline. We do not have to invent it.

It also reframes a journal result: `perf.md` closed "per-site read ICs" as flat on both engines. That closed the *fast path*; it does not falsify a per-site **feedback record** with a state machine, which is what stage O adds.

### 3.3 Trial inlining: caller-specialized feedback

`js/src/jit/TrialInlining.h` (the `[SMDOC] Trial Inlining` block) is the mechanism we have nothing like. A function with several callers may be polymorphic overall but monomorphic per caller; so baseline execution allocates a **fresh `ICScript` per call site** and replaces that one call IC with a specialized one, while other callers keep the default. Warp then compiles the callee using the caller-private IC data, recursing.

The lesson: **specialize feedback per caller, not just per callee.** This is the concrete shape of our inlining stage, and it pairs exactly with retirement — specialize per caller, invalidate to re-specialize, never bail.

### 3.4 Warp: main-thread snapshot, off-thread build

`WarpOracle` snapshots bytecode, Baseline CacheIR and stub data on the main thread (even copying nursery objects, because it cannot GC), then `WarpBuilder` builds MIR off-thread. Read but **not taken**: our compile is on the calling thread and reads live heap. It is real, and gated on measured compile latency.

### 3.5 Bailout reconstructs the *Baseline Interpreter* frame

The docs say it plainly: a bailout "reconstructs the native machine stack frame to match the layout used by the Baseline Interpreter." This is `DISPATCH_DEOPT`, and it is *cheaper* for us because there is no side table to consult — our compiled frame already mirrors the interpreter stack. SM validates the design **and names the exact condition under which we would need their machinery**: the day the compiled frame diverges (deep inlining, escape analysis). Hence the §2 invariant.

### 3.6 The wasm `Instance`

`spidermonkey-wasm.md`: one **register-held pointer per module** (`TlsData`), hot fields first in an asserted compact-offset region, globals inlined into it, GC state (nursery position, store buffer, barrier address, alloc sites) reachable, compiled frames traced by per-code-range `StackMaps`. This is the antidote to our worst measured cost — we rebuild a ~40-field `JitCallContext` every call. `call-frame-plan.md` §3.4 already aims at this shape; this document adopts it as the context target.

### 3.7 SM deletes

It removed `IonBuilder` and engine-global Type Inference; Warp beat Ion partly by *recompiling less*. V8 accumulates; SM prunes. Pruning is our natural posture as a young engine: we add the smallest thing that works and we never let a whole-program pass in.

## 4. What V8 and JavaScriptCore still teach

Kept from the earlier draft; the secondary lessons.

- **The contract between tiers is the hard part, not the optimizer.** CacheIR exists because per-tier IC logic duplicated and drifted; JSC's retrospective: "speculation is hard to maintain." One op catalog, single-sourced.
- **Inlining is early, not late.** It is the highest-ROI transform and the enabler of escape analysis.
- **A coarse typer is enough.** V8's lattice, JSC's 40-bit `SpeculatedType`, SM's `MIRType`: a handful of cases (Smi / HeapNumber / String / Object / Hole) captures most of the win.
- **A memory-effect oracle makes GVN/LICM sound.** JSC's `DFGClobberize.h` over an `AbstractHeap` hierarchy; we need our own before any hoisting.
- **Don't over-tier.** SM's Warp beat Ion by recompiling less; JSC's guidance is to skip tiers and OSR-entry until a two-tier model is at its ceiling. Every engine's rewrite was driven by maintenance and compile time, not peak throughput.

## 5. The design

```
Step interpreter  +  per-site feedback records      (guards + ops, as data)
   -> JS-level SSA CFG IR                            (Maglev/Turboshaft/B3-like, NOT SoN)
   -> inline (trial, caller-specialized) -> type -> escape -> GVN/LICM/load-elim
   -> lower to Cranelift                             (ISel, regalloc, machine opts)
   premise dies -> retire the body, resume the interpreter   (no deopt, no side table)
```

Design decisions, each grounded above:

- **A JS-level SSA CFG IR, not sea-of-nodes.** Every engine that started with SoN left it; the CFG keeps passes readable and compile time bounded, and it is the shape Cranelift consumes.
- **Feedback as data, single-sourced.** A per-site record with typed guard+op entries and an `ICState`-style mode and budget. It replaces nothing: the existing cells stay as the invalidation substrate it validates against.
- **Retirement first, and it is small.** A body carries the premise generations it was compiled under; invalidation retires it. No frame-state, no translation encoder.
- **Keep Cranelift.** No custom ISel, no custom regalloc. The backend question is the closing 2–3x and belongs to a later top tier.

## 6. The plan

Each stage is probe-first, lands on both engines where applicable, and owes the full gate. Order matters: O gates B, I, E, L; B is the first measured win; C0/C1 (`call-frame-plan.md`) gate I and E.

| stage | content | measured targets (2026-10-03) | needs |
|---|---|---|---|
| **O** | per-site feedback records (CacheIR-shaped data) + `ICState` valve + the retire hook | none (enabling) | — |
| **B** | builtin intrinsic inlining (scalar intrinsics first, then array/collection) | `math_abs` 72x, `string_charat` 65x, `regexp_test` 31x, `set_has`/`map_get` ~29x, `array_indexof`, `array_slice` | O |
| **I** | trial inlining: caller-specialized records, inline a monomorphic callee's Steps under a size budget, recursing | `method_call` 6x, `js_call` 3.6x, `proto_method_call`, `own_builtin_call`, hof rows | O, C1 |
| **E** | escape analysis + scalar replacement (non-speculative: no `Recover_*` needed) | `object_keys` 234x, `typed_array_for_each` 46x, `array_alloc`/`object_alloc` 20x, `destructure`, `construct_churn`, `json_*` | I |
| **L** | GVN + LICM + load elimination over the SSA IR, with a `clobberize`-style effect oracle | `obj_prop` 23x, `prim_prop` 24x, `element_read/write`, `array_at` | O |
| **T** | coarse typer to drive guard elision | across I/E/L | L |

**Stage O — the substrate and the retire hook.** A per-site record keyed by `(body, step)`: a small typed log of what the site has seen (shape / element kind / callee identity / operand hint) plus the mode and budget. The interpreter's member, global, call and arithmetic handlers write it; the compiled tier's slow paths update it on a miss. Invalidation reuses the generations and epochs that already exist (member/global value cells, `leaf_gen`). Retirement is a hook on that invalidation: a compiled body lists its premises, and a hit retires the body. Probe: count records written, and count retirements that would fire on the corpus, before any transform consumes them. Gate: everything, with no perf claim.

**Stage B — builtin intrinsics.** At a call site whose callee cell resolves to a known builtin, splice the intrinsic in place of the call; a write to that cell retires the body. Scalar intrinsics first (`Math.*`, `charCodeAt`, numeric conversions) because the splice is trivial and they validate the retire mechanism; array/collection intrinsics next. Probe: the `math_abs`/`string_charat`/`indexof` rows isolated A/B, plus the retirement count under a workload that reassigns a builtin.

**Stage I — trial inlining.** Build a caller-private feedback record at a call site, then inline the callee's `Step`s under a size budget (start from SM's/V8's numbers: ~30 B "small", a few hundred B total). Probe: how many corpus call sites are monomorphic and under the budget. Gate: the call rows fall, corpus parity 0, and the differential tests for a callee with captures, `this`, `arguments`, a tail call and a throw.

**Stage E — escape analysis + scalar replacement.** An object that does not escape a wholly-compiled body becomes SSA values; field loads/stores fold. Because there is no deopt, there is no `Recover_*` materialization to build. Probe: the allocation count per corpus row (`--gc-trace`) before and after. Gate: the alloc and `object_keys` rows fall, and no row regresses on the jit-loss gate.

**Stage L — GVN/LICM/load elimination.** Needs the effect oracle first; without it, hoisting is unsound. Probe: the invariant-load count per corpus row. Gate: the read rows fall.

**Stage T — the typer.** A small lattice on SSA values to eliminate redundant guards and pick representations. Probe: the guard count per site.

**Outside this plan: the baseline numeric lowering.** `perf-findings.md` §2 covers a hole the stages do not. The int32 gap is **not** a missing operator — G9 already inlined the six integer ops; it is that the fast register lane (`plan_loop_num`/`rhs_recipe`/`NumRhs`) is f64-only, so a bitwise-wrapped RMW never enters it and the inline's per-op range guard plus the `Value` round-trip are paid every iteration. **A1 landed 2026-10-03**: `plan_loop_int` runs `s = (s <arith> rhs) | 0` / `& mask` as wrapping i32 in the shared `loop_num` register (7.1 → 2.75 ns), guarded-free because the plan proves the rhs int32. A2 (a guarded rhs for a slot or an unbounded counter) remains. `%` is not inlined by design (cranelift 0.134 has no `frem`); a faster `%` needs a direct helper and, for SM parity, an integer `srem` path.

**Deferred, deliberately:** off-thread compilation (SM §3.4) and OSR *entry* until a function-level tier is at its ceiling; a mid-tier until compile time is the constraint; any engine-global type inference (SM deleted it); and the backend work (a top tier, not a swap) until the front end is closed.

## 7. Traps

- **Retire thrash.** A hot polymorphic site can recompile more than it runs. This is *the* risk of the whole design; the `ICState` budget and the count probe in stage O exist to catch it before stage B ships.
- **The frame invariant.** The moment inlining or EA lets the compiled frame diverge from the interpreter frame, `DISPATCH_DEOPT` stops being a resume and starts needing a side table (SM `Snapshots.h`). Keep them identical; if a stage cannot, that stage owes the snapshot machinery and must say so.
- **GC roots in optimized frames.** SSA values are not on the conservative stack scan. A live SSA heap reference, and anything EA sinks, must be traceable (`jit_roots`) or materialized before a safepoint.
- **The leaf lane.** `run_jit_leaf` / `run_inline_leaf` run an entry directly and do not handle `DISPATCH_DEOPT`; a specialized body must not be leaf-compiled until that lane does.
- **`JIT_MAX_COMPILE_STEPS`** (128 debug / 1024 release) silently returns an oversized body to the interpreter; inlining grows bodies. Measure body sizes as I lands.
- **Per-tier semantic drift** is what CacheIR exists to prevent; the op catalog stays single-sourced (the lift, the passes and the lowering all target the same op type).
- **`e.stack`, `using` disposal, inlined-frame naming** have each broken once. Retirement makes resumption the common path, so a resume must land at a step boundary with the operand stack exactly as the interpreter expects.
- **The helper ABI is a four-file mirror.** Reuse it; do not fork a private copy for the IR lowering.
- **The interpreter is the oracle.** It stays at baseline fixture counts throughout; the JIT is faster, never different.

## 8. Definition of done

- A specialized body retires correctly when a premise cell is written, under `--gc-stress`, and the interpreter resumes at the right step.
- Builtin intrinsics, trial inlining, escape analysis and LICM each move their target rows on the corpus, with the jit-loss gate green and test262 (language + built-ins) and the wasm suites at baseline. A perf change that costs a fixture is reverted.
- Retirement did not thrash: the corpus retirement count is small and the megamorphic valve demonstrably engages.
- A written statement of what remains between this tier and a V8/SM-class JIT — the 2–3x backend and the top-tier question.

## 9. Open decisions

1. **Feedback store location.** Per body (`CompiledBody`) or per closure (the function object), and whether the caller-private trial-inlining record lives beside it. Decide at O; SM keeps it per site in a stub, V8 per closure.
2. **SSA IR vs extending the `Step` stream in place.** This plan assumes a JS-level SSA IR for the passes. The cheaper first step is feedback-driven guards emitted straight to Cranelift, with the SSA IR built only when inlining needs it.
3. **How a premise is recorded.** A per-body premise set (a list of cells/generations) vs a per-site one; the first is simpler to retire, the second is what trial inlining wants.
4. **Tier count.** One optimizing tier to start; a mid-tier only on a measured compile-time constraint.

## 10. Relationship to the other plans

- **`call-frame-plan.md`** supplies the invariant this plan leans on (one frame) and the context target (§3.6, the wasm `Instance`). C0/C1 gate stages I and E.
- **`jit-quality-plan.md`** is the executed predecessor; its §6 "not a TurboFan rewrite" boundary is superseded here, as the earlier draft said, with SM as the reference.
- **`spidermonkey-study.md`** and **`spidermonkey-wasm.md`** are the source notes for §3.
- **`perf-findings.md`** is the wider gap map and the baseline numeric hole (§2 above) that sits outside the stage order.
- **`optimizing-tier-impl.md`** is the engineering companion; it is reconciled with this plan (the IR lives at `crates/jit/src/opt/`, the increments carry the stage letters, and I0 has landed).
