# SpiderMonkey's wasm: what it does that we don't

We had skimmed SM (§2.3 of `optimizing-tier-plan.md`). This is the real
look, from `js/src/wasm/{WasmBaselineCompile.h,WasmGenerator.h,WasmCompile.h,
WasmInstance.h}` and `js/src/jit/JitFrames.h`. It matters because SM's wasm
tiering is cleaner than V8's, and because its `Instance` is the register-held
context our call/frame plan is trying to invent — better than we had planned.

## 1. The tier model: baseline + Ion, one frame, shared stubs

`Tier1` is `BaselineCompileFunctions` ("Generate adequate code quickly"); it
is a **one-pass compiler with no IR**: `BaseLocalIter` walks the locals and
hands each a **frame offset** (the class doc says the iteration is "the
property of the `BaseStackFrame`"), so wasm locals live in fixed frame slots
exactly as Ignition/Sparkplug do for JS. `Tier2` is Ion, reached through
`CompileMode`/`CompileState` (`Once`, `EagerTier1`, `LazyTier1`, `Tier2`).

The key: `ModuleGenerator::prepareTier1` emits **one code block of stubs
shared between the tiers** (`CompiledCode` carries `codeRanges`, `stackMaps`,
`trapSites`, `tryNotes`, `callSites`, `callSiteTargets`,
`codeRangeUnwindInfos`), so tier 1 and tier 2 agree on the frame, the traps
and the call protocol. There is no per-tier ABI.

## 2. Partial tiering: recompile one function, patch its callers

`startPartialTier(funcIndex)` / `CompilePartialTier2(code, funcIndex)` tier
up **a single function**, not the module. The mechanism is in the generator:
`CallFarJumpVector callFarJumps_`, `CallSiteTargetVector callSiteTargets_`,
`startOfUnpatchedCallsites_`, `lastPatchedCallSite_` — call sites are emitted
as patchable far jumps, and tier-up rewrites the target. On the instance:
`jumpTable_` ("one entry for each baseline-compiled function" — the indirect
call table), `requestTierUpStub_`, and `callRefMetrics_`/`updateCallRefMetricsStub_`
("used with lazy tiering for collecting speculative inlining information").

So SM's hotness is **per function**, with a real dispatch table and call-site
patch points, and tier-up collects inlining hints. That is strictly finer
than a body-level compile threshold and is what makes "compile the hot
function, leave the rest interpreted" sound.

## 3. Compilation as a stream, in parallel

`CompileStreaming` takes a pre-sized code section and the bytes-so-far, and
compiles tier 1 **as the stream arrives** (`ExclusiveBytesPtr`/`StreamEndData`
with a `CompleteTier2Listener`); tier 2 runs after. `CompileTask`s are
batched by bytecode (`batchedBytecode_`) onto helper threads
(`HelperThreadTask`), with a `finished_`/`numFailed_`/`condVar_` handshake.
Neither is a marginal optimization: they are why SM's instantiation is not a
stop-the-world compile.

## 4. The `Instance` (= `TlsData`): a register-held context

This is the piece our call/frame plan should copy. An `Instance` is ~one per
module, held in a register by compiled code, with **the hot fields first and
an asserted compact-offset region** (`offsetOfLastCommonJitField()` — "the
first fields … are reserved for commonly accessed data from the JIT, such
that they have as small an offset as possible"):

- `memory0Base_`, `memory0BoundsCheckLimit_` (memory 0, for bounds-checked
  loads), `debugStub_`, `realm_`, `cx_`, `pendingException_(_Tag)`,
  `stackLimit_`, `interrupt_`, `onSuspendableStack_`,
  `addressOfNeedsIncrementalBarrier_`, `allocSites_`;
- then `jumpTable_`, 4 `baselineScratchWords_`, `storeBuffer_`,
  `addressOfNurseryPosition_` — **GC state the compiled code needs to
  allocate and to run barriers inline**;
- and `MOZ_ALIGNED_DECL(16, char data_)` last, where **globals are inline in
  the instance** ("Globals for the module start here and are inline in this
  structure").

Cross-instance calls carry the callee's instance (the `calleeToken` /
frame), so a call does **not** rebuild a context: it switches one pointer.
`Instance::traceFrame` / `updateFrameForMovingGC` walk a frame with a
`StackMap` (`traceFrame(trc, wfi, nextPC, highestByteVisitedInPrevFrame)`),
so compiled frames are **precisely traced**, not conservatively scanned.

## 5. One frame across JS and wasm

`js/src/jit/JitFrames.h` defines `WasmToJSJitFrameLayout`,
`WasmGenericJitEntryFrameLayout`, `DirectWasmJitCallFrameLayout` **inheriting
`JitFrameLayout`**, and every JIT→C++ boundary is an `ExitFrameLayout` with a
typed `ExitFooterFrame` (VMFunctionId, CallNative, …). A wasm call into JS
and a JS call into wasm are the same frame shape with a different descriptor;
there is no per-boundary context object.

## 6. What is concretely better than V8

- **Baseline and optimizing tiers share the frame, the stubs and the trap
  metadata** (V8's Liftoff/TurboFan sharing is real but coarser; SM's
  `prepareTier1` + one `CompiledCode` metadata set is one ABI).
- **Function-granular tier-up with call-site patch points** (`CallFarJump`,
  `jumpTable_`, `requestTierUpStub`) and inlining hints (`callRefMetrics`).
- **Streaming and parallel compilation as first-class** (`CompileStreaming`,
  `CompileTask`).
- **A register-held `Instance` with inline globals and GC state**, so calls
  switch a pointer and compiled code allocates/barriers inline.
- **Precise stack maps** (`StackMaps`, `traceFrame`) instead of a
  conservative scan.

## 7. What Slag should adopt

**Wasm engine (`crates/wasm`), probe-gated:**

1. **A baseline tier that shares the frame.** Our `compile.rs` is the only
   tier and compiles per body lazily on first reach (`Store::ensure_compiled`,
   landed 2026-09-14). The SM shape is: a cheap one-pass baseline that
   mirrors the interpreter's frame (so the interpreter and the baseline are
   interchangeable), Cranelift as the optimizing tier over the *same* frame
   and *same* stack maps. Our interpreter already has an explicit-frame
   operand machine (`exec/`), which is the frame to mirror.
2. **Partial tier-up.** Today compilation is per body with a null-entry
   fallback; add SM's call-site patch points so a hot body tiers up without
   recompiling the module, with a per-body jump table for direct/indirect
   calls.
3. **Precise stack maps.** We have no compiled-frame root enumeration; a
   conservative scan is the fallback. `StackMaps` per code range plus a
   `traceFrame` over the explicit frame is the SM mechanism, and it also
   fixes wasm-on-the-JS-heap root hazards.
4. Streaming/parallel compilation last; it is an instantiation-latency win,
   not a throughput one, and `many-bodies.wast` (0.25 s → 0.054 s with lazy
   compile) is the probe already.

**General engine (feeds `call-frame-plan.md`):**

5. **A register-held context with hot-fields-first, not a rebuilt struct.**
   Our `JitCallContext` is ~40 fields rebuilt per call and our `Vm` is 1096
   bytes pooled per call. SM's `Instance` shows the target shape: one
   pointer, small fixed offsets, hot fields first, GC state reachable, and
   (for JS) the frame itself carrying what was per-call. This strengthens
   §3.4 of the call/frame plan ("the context is the frame, not a struct"):
   the ctx should be a *layout*, held across the activation, with an asserted
   compact-offset head — not allocated again per call.
6. **Per-function tiering with patch points, not a body count.** The
   `JIT_COMPILE_THRESHOLD`/`jit_calls` counter is the analog of "tier up by
   count"; SM's `jumpTable_` + `requestTierUpStub` + `callRefMetrics` is the
   better architecture and belongs in the call/frame plan's tier boundary.

## 8. First probe-gated step

The cheapest piece that pays and that we can measure today: **a bytecode
fetch/decode split in the wasm interpreter** is *not* it — the SM lesson is
architectural. The first step is the one the call/frame plan already
requires: make the wasm interpreter's frame the shared frame, then a baseline
that mirrors it. Probe: `wasmtest equiv` (already 12 suites, 0 diverged) plus
`interp-hot-loop.wast`; target the compiled-vs-interpreted margin, not a
micro-probe.
