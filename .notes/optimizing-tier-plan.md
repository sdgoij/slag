# The optimizing tier: what V8, SpiderMonkey and JavaScriptCore teach, and the plan

## 1. Why this exists

The inline-coverage arc (`.notes/jit-quality-plan.md`) did what it set out to
do: the helper-share census named the hot surfaces (G1–G21), and each landed
slice moved its row. But four of those slices moved individual corpus rows
17–46% and **moved nothing else** (`jit-quality-plan.md` §6), and the design
they work inside is unchanged: compiled dispatch with inline fast paths, where
every non-trivial `Step` either inlines a narrow case or calls one of the 131
helpers (`crates/jit/src/compiler.rs:1-22`, `JIT_SLOW_PATHS`,
`crates/runtime/src/jit.rs:1159`). There is **no type feedback and no deopt**
(`jit-quality-plan.md` §2).

The 2026-10-02 re-baseline says the same thing with numbers. The top of the
gap-to-d8 table is not "our machinery is slow"; it is **V8 applying transforms
we do not have**:

| row | gap | what d8 does |
|---|---|---|
| `array_alloc` | 860x | removes the allocation (escape analysis) |
| `object_alloc` | 459x | same |
| `array_at` / `obj_prop` / `element_read` / `prim_prop` | 32–388x | hoists the invariant load out of the loop (LICM) |
| `method_call` / `js_call` | 42x / 31x | inlines the callee away |
| `destructure` | 214x | scalar-replaces both objects (`--trace-gc`: 8 scaffold scavenges then 0, against 64 jitless) |

That last column is the crux: these are **not** ratios we can close by making
a helper faster. They need inlining, escape analysis and LICM — the machinery
of an optimizing tier.

`jit-quality-plan.md` §6 declares "Not a TurboFan rewrite" out of scope. That
was correct for that plan. This plan supersedes that decision, with the source
of three engines as the reference and the measured rows as the target.

## 2. What the three engines teach

We borrow architecture, never code. All three checkouts were read for this
section (V8 locally at `v8/`, commit `39ee186b`; SpiderMonkey and
JavaScriptCore from their public trees).

### 2.1 The convergent lessons

1. **The hard part is the contract between tiers, not the optimizer.**
   SpiderMonkey's CacheIR exists because per-tier IC logic duplicated and
   drifted; Warp was "only" a new consumer of the same IR. JavaScriptCore's
   2020 retrospective: "speculation is hard to maintain" because five
   components (bytecode, control, profiling, compilation, OSR) must agree.
2. **Feedback is the input and deopt is the licence.** Inlining, escape
   analysis and LICM are sound *only* because eliminated state can be rebuilt
   (V8 `FrameState`/translation opcodes; SM `MResumePoint` + `Recover_*`; JSC
   `Check` + `MaterializeNewObject`). Both must land **before** any speculative
   transform.
3. **Inlining is early, not late.** V8's pipeline runs inlining as phase 2;
   everything downstream assumes it. It is also the highest-ROI transform and
   the enabler of (2)'s escape analysis.
4. **One op catalog as data, declared `shared`/`transpile`.** SM's
   `CacheIROps.yaml` (443 ops; 362 shared across tiers, 434 transpilable into
   MIR) is the mechanism that keeps semantics from forking per tier.
5. **A coarse typer is enough.** V8's lattice, JSC's 40-bit `SpeculatedType`,
   SM's `MIRType` — a handful of cases (Smi / HeapNumber / String / Object /
   Hole) captures most of the win.
6. **Don't over-tier and don't overspecialize.** SM's Warp beat Ion largely by
   *recompiling less*; JSC's own guidance is to skip four tiers and OSR-entry
   until a two-tier model is at its ceiling. Every engine's rewrite
   (Crankshaft→TurboFan, ~4 years to pay off; SoN→Turboshaft; Ion→Warp) was
   driven by **maintenance and compile time**, not peak throughput.

### 2.2 V8 — TurboFan, Turboshaft, Maglev

Pipeline (`v8/src/compiler/pipeline.cc`): graph build → **inline** → trim →
typer → typed lowering → loop peeling → load elimination → **escape analysis**
→ simplified lowering (representations) → scheduling → codegen. The rewriters
have since moved to **Turboshaft**, a CFG IR with copy-based phases and
`OpEffects` bitmasks, and the default front end is **Turbolev** (Maglev's
builder feeding Turboshaft). Sea-of-Nodes was left behind deliberately
(`v8/docs/compiler/why-cfg.md`, "Land ahoy: leaving the Sea of Nodes").

Feedback → specialization → deopt: Ignition's `FeedbackVector` records what
each site saw; `js-typed-lowering.cc` turns a `kSignedSmall` hint into a
`SpeculativeNumberAdd` behind an overflow guard; every guardable node carries a
`FrameState` (`frame-states.h:184`) describing the *unoptimized* frame, and the
compiled deopt info is a compact opcode stream
(`deoptimizer/translation-opcode.h`). Inlining budgets live in
`js-inlining-heuristic.cc` + `flag-definitions.h` (~460 B single / 920 B
cumulative, small functions bypass the caps).

Maglev's lesson is the tiering argument: "fully optimized" and "fast to
compile" are different products; a single IR level, no instruction selector,
and a linear-scan regalloc are enough to speculate sooner on more functions.

### 2.3 SpiderMonkey — Ion and Warp, around CacheIR

The pipeline is Interpreter → Baseline interpreter → Baseline JIT → Warp/Ion,
tier-up by warm-up counters (10/100/1500, `JitOptions.cpp`), with OSR entry at
loop headers.

CacheIR (`CacheIR.h`, `[SMDOC] CacheIR`) is a linear guard+op IR per site —
`GuardToObject`, `GuardShape objId, shapeOffset`, `LoadFixedSlotResult`, ... —
where operands are small typed IDs and **references are stub-data offsets**
(`shapeOffset`), so many sites share one compiled stub. `CacheIRWriter` records
it; `CacheIRReader` reads it at compile time or from `CacheIRStubInfo` at
runtime; `CacheIRCompiler` emits it.

Warp is three phases: `WarpOracle` snapshots bytecode + Baseline CacheIR +
stub data on the main thread (cheap, validates invariants); `WarpBuilder`
builds MIR off-thread; `WarpCacheIRTranspiler` mechanically lowers `CacheOp` →
`MDefinition*` (e.g. `emitGuardShape` is ~5 lines). The old `IonBuilder` was
removed with **Type Inference** — a whole-program type-accumulation pass that
taxed every function to help a few and forced main-thread building.

MIR is a typed SSA CFG (`MIR.h`, `MIRType resultType_` per def), with
`TypeAnalyzer`/phi specialization, GVN, LICM, range analysis, and
`ScalarReplacement.cpp`. Bailout correctness is the contract: `isRecoverableOperand`
gates what may be eliminated, resume points record `rp->addStore(...)`, and
`Recover_*` instructions rebuild eliminated values (`Snapshots.h`,
`Bailouts.cpp`). `TrialInlining.cpp` pre-builds caller-specialized ICs so
inlining is context-sensitive without Warp owning a heuristic.

### 2.4 JavaScriptCore — DFG, FTL and B3

Tiers: LLInt → Baseline → DFG (speculative, fast-compile, CPS, **no SSA**) →
FTL (DFG SSA → **B3** → Air). Tier-up is an execution counter with dynamic
thresholds and exponential backoff; speculation is expressed as **OSR exit**,
not a diamond — a failed check tail-calls to code that restores the baseline
frame, so the check is not repeated. The bet is explicit: `EV = p*B - (1-p)*C`
with `B ≈ 1.5 ns` and `C ≈ 2.5–10 µs`, i.e. **speculate only when failure is
believed impossible**.

`DFGClobberize.h` is the memory-effect oracle that makes CSE and LICM sound:
effects over an `AbstractHeap` hierarchy, `def(HeapLocation(...))`, and
`clobberTop() = read(World); write(Heap)` for calls. `DFGObjectAllocationSinkingPhase.cpp`
does escape analysis with must-points-to and **phantom nodes** +
`ExitTimeObjectMaterialization` so an OSR exit can rebuild a sunk object. B3
(`b3/B3Procedure.h`) is a deliberately cheaper SSA IR than LLVM (4.7x faster
compile) with `ReduceStrength`, `HoistLoopInvariantValues` (LICM),
`FoldPathConstants`; `Check` and `Patchpoint` are first-class to avoid LLVM's
exit-block explosion.

## 3. The design for Slag

The whole backend is already solved by Cranelift: instruction selection,
register allocation, and machine-level GVN/LICM. What is missing is the
**front end**, and the three engines agree on its shape:

```
Step interpreter + per-site feedback
   -> typed feedback records        (a CacheIR-like guard+op stream per site)
   -> JS-level SSA CFG IR           (Maglev/Turboshaft/B3-like, NOT SoN)
   -> inline -> type -> escape -> GVN/LICM/load-elim
   -> lower to Cranelift            (ISel, regalloc, machine opts)
   deopt: a failed guard rebuilds the interpreter frame from recorded state
```

Design decisions, each grounded in §2:

- **A JS-level SSA CFG IR, not sea-of-nodes.** Every engine that started with
  SoN left it; the CFG keeps passes readable and compile time bounded. It is
  also the shape Cranelift consumes.
- **Feedback as data, not bespoke arms.** A per-site record with typed guards
  (shape, element kind, callee identity, operand hint) and a megamorphic escape
  valve, with the op catalog declared once and marked `shared`/`transpile`
  (SM's mechanism). This replaces the current global direct-mapped cells
  (`MEMBER_CELLS = 16` `ir.rs:2316`, `GLOBAL_CELLS = 256` `ir.rs:2687`,
  `LEAF_CACHE = 256` `ir.rs:2700`) with a per-site record — but those cells are
  the *invalidation substrate* and stay.
- **Deopt first.** A frame-state per guardable op (bytecode offset + where each
  interpreter value lives) plus a reconstruction table, and a Cranelift `brif`
  to a deopt block that hands back to the interpreter. This is the single
  biggest new subsystem and gives no speed on its own.
- **Keep Cranelift.** No custom ISel, no custom regalloc, no scheduling IR.

## 4. The plan

Each stage is probe-first, lands on both engines where applicable, and owes the
full gate (`jit-quality-plan.md` §5's list). Order matters: O gates I, I gates
E and L.

| stage | content | measured targets (2026-10-02) |
|---|---|---|
| **O** | feedback records + deopt substrate | none (enabling) |
| **I** | caller-specialized inlining (SM `TrialInlining` analog) | `method_call`, `js_call`, `hof_methods`, `apply_call`, `closure_capture`, `generator_loop` |
| **E** | escape analysis + scalar replacement | `destructure` 214x, `construct_churn`, `spread_assign`, `object_keys`, `json_*` |
| **L** | GVN + LICM + load elimination over the SSA IR | `obj_prop`, `element_read/write`, `array_at`, `warm_store`, `typed_array`, `many_objects_read` |
| **T** | coarse typer to drive guard elision | across I/E/L |

**Stage O — the substrate.** Feedback: a per-site `Vec<u8>` of typed
guard+op records, keyed by (function, step index), validated by the existing
generations/epochs; a writer at each call/member/element/arith site in the
interpreter and the compiled tier. Deopt: a `FrameState` per guardable op and a
translation encoder, so a failed guard tail-calls back. Probe: count the
records written and the guards that would fire on the corpus, before any
transform consumes them. Gate: the workspace tests, both test262 areas, the
eight wasm suites, corpus parity 0, and the jit-loss gate — with no perf claim.

**Stage I — inlining.** Inline a monomorphic call site's callee into the SSA IR
with a size budget (start from SM's/V8's numbers: ~30 B "small", a few hundred
B total). Probe: how many corpus call sites are monomorphic and under the
budget. Gate: `--jit-bench` `non-leaf call`/`builtin call`/`method call` fall,
corpus parity 0, and the differential tests for a callee with captures, `this`,
`arguments`, a tail call and a throw.

**Stage E — escape analysis + scalar replacement.** Objects that do not escape
become SSA values; field loads/stores fold; a materialization recipe lets a
deopt rebuild them. Probe: the allocation count per corpus row (`--gc-trace`)
before and after, and the `destructure` GC trace as the reference. Gate:
`destructure` and `construct_churn` fall, and no row regresses on the jit-loss
gate.

**Stage L — GVN/LICM/load elimination.** Needs a memory-effect oracle in the
JSC `clobberize` style (an `AbstractHeap`-like hierarchy and per-op read/write
sets) — without it, hoisting is unsound. Probe: the invariant-load count per
corpus row. Gate: `obj_prop`, `element_read`, `array_at` fall.

**Stage T — the typer.** A small lattice on SSA values to eliminate redundant
guards and pick representations; it is what makes E and L fire more often.
Probe: the guard count per site before/after.

**Cross-cutting, deliberately deferred** (per §2.6 and JSC's "what to skip"):
OSR *entry* (start with function-level tier-up; the interpreter already runs
loops fast), a second mid-tier (Maglev-style) until compile time is the
constraint, off-thread compilation until compile latency is measured to matter,
and any engine-global type inference (SM deleted it for good reason).

## 5. Traps

- **Deopt fidelity.** The current compiled lane already had a strict-tail-call
  bug from sharing a caller's `Vm` (`jit-quality-plan.md` §8, Cut 1); an
  optimizing tier makes every guard a new way to resume wrongly. `e.stack`,
  `using` disposal, and inlined-frame naming are the three that already broke
  once.
- **Per-tier semantic drift** is the failure CacheIR exists to prevent; keep
  the op catalog single-sourced.
- **GC roots inside optimized frames.** Escape-analyzed objects and pooled Vms
  must stay traceable, or a mid-run collection loses them (the conservative
  stack scan cannot see SSA values).
- **The compile-size cap.** `JIT_MAX_COMPILE_STEPS` (128 debug / 1024 release,
  `crates/runtime/src/jit.rs:1354`) exists for a reason; inline expansion grows
  bodies and can push a row back onto the interpreter, silently.
- **Measure at release**, interleaved A/B min-of-3, and re-take the baseline —
  the corpus TSVs go stale behind identical row names (`tools/corpus`'s
  manifest exists for exactly that).

## 6. Definition of done

- The feedback + deopt substrate exists and a guard can fire and resume
  correctly under `--gc-stress`.
- Inlining, escape analysis and LICM each move their target rows on the corpus,
  with the jit-loss gate green and test262 at baseline (a perf change that
  costs a fixture is reverted).
- A written statement of what remains between this tier and a V8-class JIT.

## 7. Open decisions

1. **SSA IR vs extending the `Step` stream in place.** The plan assumes a new
   SSA IR; a cheaper first step is to keep emitting to Cranelift but add
   feedback-driven guards, and only build the SSA IR when inlining needs it.
2. **Where the feedback lives.** In `CompiledBody` (per body, cache-friendly)
   or on the function object (per closure, shareable across closures). SM keys
   CacheIR per site in a stub; V8 per closure feedback vector.
3. **Tier count.** One optimizing tier to start; add the Maglev-style mid-tier
   only on a measured compile-time constraint.
