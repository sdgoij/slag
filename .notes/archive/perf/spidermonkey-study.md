# SpiderMonkey as the primary source

We built the engine reading V8. `spidermonkey-wasm.md` showed SM's wasm
tiering is cleaner; this is the whole-engine look, from `js/src/jit/*`,
`js/src/doc/*`, and the `[SMDOC]` comments (SM's in-tree documentation
system — `js/src/doc/index.rst`). It is a better model for us than V8 in
several places, and two of its mechanisms change what our plans should say.

## 1. Four tiers, not two — and the baselines share ICs

`js/src/doc/index.rst` names them:

1. **Interpreter** — C++ bytecode (`vm/Interpreter.cpp`).
2. **Baseline Interpreter** — "a hybrid interpreter/JIT that interprets the
   bytecode one opcode at a time, but **attaches small fragments of code
   called Inline Caches (ICs)**" that speed up the same opcode next time.
3. **Baseline Compiler** — "the same IC mechanism from the Baseline
   Interpreter but additionally translates the entire bytecode to native
   machine code… This machine code still calls back into C++ for complex
   operations."
4. **WarpMonkey** — inlines other scripts, specializes on observed data,
   MIR → LIR → codegen, with **bailouts**.

The load-bearing decision is that **the baseline interpreter and the baseline
compiler are the same tier for IC purposes** — one IC mechanism, so promoting
from interpreting to compiled keeps every IC. V8's Liftoff/Sparkplug share a
frame; SM additionally shares the *feedback*. That is why its tiering is
cheap and why Warp's input is rich immediately.

## 2. ICs are first-class per-site stubs with a state machine

`js/src/jit/ICState.h` is small and worth copying outright. Per IC site:

- `Mode { Specialized → Megamorphic → Generic }`; the comment: at the max
  stub count "we discard all stubs and transition the IC to Megamorphic to
  attach stubs that are more generic… If we again attach the maximum number
  of stubs, we transition to Generic." `MaxOptimizedStubs = 6`.
- Failure tracking with an *adaptive* retry budget:
  `maxFailures() = 5 + 40 * numOptimizedStubs` — a site that has attached
  stubs gets many more attempts before it gives up. `trackAttached` resets
  `numFailures_` to 1, not 0, "so that code which inspects state can
  distinguish no-failures from rare-failures".
- A `mayHaveFoldedStub` hint (folded stubs can be swept later, so it is a
  hint, not exact).
- `usedByTranspiler` — set when `WarpOracle` built a snapshot from this IC.
- `TrialInliningState { Initial → Candidate → Inlined | MonomorphicInlined }`
  with a debug assertion that the transition is monotone and happens "at most
  once per IC site".

**This reframes a result in our journal.** `perf.md` closed "per-site read
ICs" as flat on both engines. SM's ICs are not only for the read fast path —
they are the *feedback substrate that Warp and the inliner consume*. Our
falsification was about the fast path; it does not falsify a per-site
**feedback record** with a state machine, which is exactly Stage O of
`optimizing-tier-plan.md`.

## 3. Trial inlining: caller-specialized feedback

`js/src/jit/TrialInlining.h` has the `[SMDOC] Trial Inlining` block, and it is
the mechanism we have nothing like:

> "Functions with multiple callers complicate this. An IC in such a function
> might be monomorphic for any given caller, but polymorphic overall… To solve
> this, we do trial inlining. During baseline execution, we identify call
> sites for which it would be useful to have more precise inlining data. For
> each such call site, we allocate a fresh ICScript and replace the existing
> call IC with a new specialized IC that invokes the callee using the new
> ICScript. Other callers of the callee will continue using the default
> ICScript. When we eventually Warp-compile the script, we can generate code
> for the callee using the IC information in our private ICScript, which is
> specialized for its caller."

`InliningRoot` (owned by a `JitScript`) owns the candidate `ICScript`s and
tracks `totalBytecodeSize_`; `TrialInliner::maybeInlineCall` /
`maybeInlineGetter` / `maybeInlineSetter` clone the stub's shared prefix
(`cloneSharedPrefix`) into a fresh `CacheIRWriter` and re-target the call;
`FindInlinableCallData` / `InlinableCallData { calleeOperand, callFlags }`
describe what an inlinable stub looks like. The same approach recurses.

The lesson: **specialize feedback per caller, not just per callee.** Our
Stage I inlining plan talks about "caller-specialized inlining (SM
`TrialInlining` analog)" — this is the concrete shape of it: a cloned,
caller-private feedback record that makes the callee's ICs monomorphic.

## 4. Warp: main-thread snapshot, off-thread build

`WarpOracle.h` builds a `WarpSnapshot` "used by WarpBuilder to generate the
MIR graph off-thread". The oracle copies **nursery objects into the
snapshot** (`nurseryObjects_` + a dedup `nurseryObjectsMap_`, because "WarpOracle
can't GC"), records `WarpBailoutInfo`, zone stubs, script snapshots, and
`accumulatedBytecodeSize` (a compile-budget). Then `WarpBuilder` runs
off-thread and `WarpCacheIRTranspiler` mechanically lowers CacheIR → MIR.

The split is the point: all the GC-sensitive reads happen on the main thread
and are copied into a snapshot; the expensive graph build is off-thread and
touches no heap. Our JIT compiles on the calling thread and reads live heap;
that is a real (if later) architectural difference.

## 5. Bailout reconstructs the *Baseline Interpreter* frame

The doc says it plainly: a bailout "reconstructs the native machine stack
frame to match the layout used by the **Baseline Interpreter** and then
branches to that interpreter as though we were running it all along. Building
this stack frame may use special side-table saved by Warp to reconstruct
values that are not otherwise available."

Our `DISPATCH_DEOPT` (spill the working region into `vm.stack`, set `vm.ip`,
resume `run_inner`) is the same idea — and it is *cheaper* for us because our
compiled frame already mirrors the interpreter stack, so we need no side
table. SM validates the design; it also warns that the day we diverge the
compiled frame from the interpreter frame (inlining, escape analysis) we owe
a side table like theirs (`Snapshots.h`, `Recover.h`).

## 6. Stencil and lazy parsing

`js/src/doc/index.rst`: the parser produces a **Stencil** which "does not
utilize the Garbage Collector" and can be instantiated into GC *Cells* later;
parsing is *lazy* by default and a lazily parsed function is *delazified* on
first execution. This is a compilation-architecture decision we have not
made: our parse/compile artifacts are GC things, and there is no
delazification.

## 7. Objects and shapes

`NativeObject` stores property *values* and points to a `Shape` for the
*keys*; "Similar objects point to the same Shape… allows the JITs to quickly
work with objects similar to ones it has seen before." That is the same
map/shape model `perf.md`'s L1c storage decision was circling; SM's version
is field-authoritative (Option 3) with the *values* on the object and the
*keys* on the shape.

## 8. What Slag should take

**Into `optimizing-tier-plan.md` / the inlining stage:**

1. **Per-site feedback with SM's `ICState`**, not a flat cache: a mode
   (specialized/megamorphic/generic), an adaptive failure budget, and a
   monomorphic-hint. Our `MEMBER_CELLS`/`GLOBAL_CELLS` are direct-mapped
   fast-path caches; the feedback record is a different artifact and is
   justified even though the read fast path measured flat.
2. **Trial inlining as the inlining shape**: caller-specialized feedback
   (a private record per call site) so the callee's feedback is monomorphic
   at that site, replaced in place, recursing. This is Stage I's design.
3. **Two baselines sharing feedback and frame** (SM) reinforces the
   call-frame plan's "one frame" and argues for a baseline tier over our
   certified-only JIT once the frame is shared.

**Into `call-frame-plan.md`:**

4. The bailout-is-a-frame-rebuild principle (SM §5) confirms `DISPATCH_DEOPT`
   and predicts exactly when we will need a side table: once inlining/escape
   make the compiled frame diverge from the interpreter frame.

**Into the wasm plan:** see `spidermonkey-wasm.md` §7.

**Not taken (yet):** off-thread compilation and Stencil — both real, both
gated on measured compile latency and on a GC-free parse artifact, neither a
priority until the frame/feedback work lands.

## 9. Not yet studied (the next passes)

- `MIR-optimizations/` (`js/src/doc/MIR-optimizations/index`) and
  `RangeAnalysis.h`, `ScalarReplacement.cpp`, `LICM.cpp`, `ValueNumbering.cpp`
  — the pass catalog, to size our own E/L/T stages.
- `hacking_tips.md` and the GC/hazard-analysis docs (`doc/gc.rst`,
  `doc/HazardAnalysis/`), for the rooting model.
- `[SMDOC] Shapes` and `NativeObject` layout, for the object-model half of
  the perf survey.
