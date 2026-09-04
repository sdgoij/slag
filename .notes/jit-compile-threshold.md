# Plan: JIT compile threshold (Cut 69)

## 1. Problem (measured)

| Build | `--jit` | no JIT |
|---|---|---|
| HEAD (Cut 68) | 79.1s | 65.9s |
| Parent (Cut 67) | 80.8s | — |

The ~13s gap is pre-existing and structural: `JitCache::lookup` compiles **every certified body that runs**, on first consult. The corpus is ~38K mostly-short fixtures, each paying a Cranelift compile for its script body + a few one-shot function bodies (~3-4 compiles × ~100µs each). The JIT only amortizes its compile cost on bodies that run many times — which the corpus rarely does.

## 2. Goal

Skip/delay compilation of bodies that won't amortize the compile cost, without slowing the hot shapes:

- **Loop bodies** (internal loop, e.g. `run()` with a `for` loop, certified scripts with a top-level loop — the Cut 65 benches): compile **on first use**.
- **Straight-line bodies** (mostly leaves): run interpreted — the interpreter's leaf-inline (`run_inline_leaf`, ~82ns/call) is already fast — and **promote to JIT after K invocations** (start K=16).

Success: `--jit` suite time approaches the interpreter baseline; hot benches unchanged; identical pass/fail results.

## 3. Design

### 3.1 The gate lives entirely in `lookup_info` (`crates/runtime/src/jit.rs`)

It's the single choke point all four JIT decision sites already call:
`run_jit_body` (general path), `run_jit_resume` (async/gen), `run_jit_leaf` (interpreter-side leaf attempt via `try_jit_leaf`), `leaf_call_probe` (compiled-caller leaf).

```rust
// in lookup_info, replacing the known==0 branch:
if ir.jit_calls.get() < JIT_COMPILE_THRESHOLD && !ir.has_loop {
    ir.jit_calls.set(ir.jit_calls.get().saturating_add(1));
    return std::ptr::null();      // NOT cached: the next consult re-counts
}
let ptr = hook.lookup(...);       // compile
ir.jit_info.set(if ptr.is_null() { 1 } else { ptr as usize });
```

The existing tri-state `jit_info` (`0` unknown / `1` sticky-unsupported / `>1` compiled) is preserved — the threshold path deliberately never writes `1`, so promotion is never blocked. All four callers already handle null correctly (→ `Interp` / `Ok(false)` / probe→`call_slow`), so **no caller changes are needed**.

### 3.2 New fields on `CompiledBody` (`crates/runtime/src/ir.rs`)

- `pub(crate) jit_calls: Cell<u32>` — saturating; shared across closures of one site (`Rc<CompiledBody>`), so counts aggregate — desired (tco-call-args' per-step closures share one body).
- `pub(crate) has_loop: bool` — computed once at body creation by `body_has_loop(&[Step])`: **any step whose `step_targets` includes an index `< its own`** (the exact back-edge rule the JIT compiler uses to seed `back_targets` in `emit_all`), **or** a `FastLoopHead`/`RunRegBody` step (the fast/register loop shapes whose back edge is implicit). The scan lives in ir.rs and mirrors `crates/jit/src/compiler.rs::step_targets` — keep the two in sync (the runtime owns the `Step` enum).
- Set at **both** construction sites: `compile_body` (functions) and `compile_statements` (scripts, cached per source in `eval_program`'s `script_bodies` — repeated evals aggregate the counter, so eval-in-loop scripts promote).

### 3.3 Why the count works even under a compiled caller (load-bearing)

A hot leaf under a compiled caller: visit 1 probes → below threshold → probe caches a **rejection** (entry 0) → machine code routes to `call_slow` → `do_call_fast` → `fast_call_core` → `try_jit_leaf` → `run_jit_leaf` → `lookup_info` (**count++**). Every subsequent call re-consults through that chain, so the counter reaches K even though the caller's site never re-probes. After promotion, `run_jit_leaf` runs the leaf's machine code; the caller's site inlines from its **next** run (fresh ctx). Verified: `fast_call_core` L9418 → `try_jit_leaf` → `run_jit_leaf` → `lookup_info`. The Cut 68 probe-count e2e tests stay green (the site still probes once per run; the assertion is about probe count, not the leaf's execution tier).

### 3.4 Why the loop heuristic is required

A loop body runs once with many internal iterations — a pure count would never promote it. `has_loop` compiles it on first use. Residual over-compile: tiny loops (`for (i=0;i<3;i++)`, for-of over 1-2 elements) compile for little gain — acceptable; measure, and optionally add a step-count floor later.

## 4. Expected effect

The sweep overhead is dominated by one-shot script/function bodies. Under the threshold: script bodies without loops **never compile** (single eval), one-shot functions never compile. Projection: `--jit` suite ~66-70s (near the 65.9s interpreter baseline). Hot benches preserved by 3.3/3.4.

## 5. Risks & mitigations

- **Bench regression**: tco-call-args (15.9→0.26s) is a loop-less recursive body — promotes after K calls (K interpreted TCO levels ≈ nothing), then self-loops in machine code; its per-step closure bodies share the site body → aggregate count. Verify all `perf.md` hot rows before/after.
- **`jit_info == 1` stickiness**: preserved for genuinely unsupported bodies; the threshold path never writes it.
- **Eviction**: a cleared `jit_info` re-consults with `jit_calls ≥ K` (or `has_loop`) → recompiles immediately, no regression.
- **`--jit-bench` semantics**: below-threshold bodies now run interpreted — hot-loop rows unaffected; document the change.
- **Counter overflow**: `saturating_add`.

## 6. Implementation steps (ordered)

1. `ir.rs` — `jit_calls`/`has_loop` on `CompiledBody`, `body_has_loop`, set at both construction sites.
2. `jit.rs` — `pub(crate) const JIT_COMPILE_THRESHOLD: u32 = 16;` + the `lookup_info` gate.
3. Tests:
   - Unit: straight-line body `<K` calls → `lookup_info` null, `compiled_count` 0; `≥K` → compiled. Loop body → compiled on first consult.
   - E2e (`with_jit_agent`): a script calling a straight-line fn 8× (not compiled) vs 16+× (compiled); a loop body compiles on first call; re-run the two Cut 68 probe-count tests.
4. Bench: `perf.md` hot rows before/after.
5. Sweep: time `--jit` before/after; re-run language/annexB/built-ins for pass/fail/hang parity (must be unchanged).
6. Tune K (4/16/64) if warranted.
7. Docs: `.notes/jit-report.md` row 40 + TODO; `slag-jit` skill trap (the gate in `lookup_info`, the `body_has_loop`↔`step_targets` sync, the `call_slow→try_jit_leaf` promotion chain).
8. Commit message.

## 7. Measurement plan (defensible evidence)

- **Primary**: full-suite wall time, `--jit` vs no-JIT, before vs after — same bash `time` method (the PowerShell wrapper inflated the JIT side last time).
- **Secondary**: a temporary `SLAG_JIT_STATS` env hook printing the compiled-body count per run (same pattern as the removed probe counters) to verify the compile-volume drop directly, then remove it.
- **Correctness**: the three area sweeps' pass/fail/hang sets unchanged — the threshold changes *which* bodies compile, never semantics, but verify.
