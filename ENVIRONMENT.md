# Environment variables

Slag reads its tuning and diagnostic switches from the process environment.
Two conventions apply throughout:

- **Disable switches** (`SLAG_*`) are on by default; a value of `0` turns the
  feature off, and unset leaves the default. A few are **opt-in** — off unless
  set to a non-`0` value — and are marked as such.
- **Diagnostic switches** (`JIT_*`, `SLAG_JIT_DEOPT_*`) are off unless set, and
  any set value enables them (`SLAG_JIT_DEOPT_PROBE` parses its value as a step
  number).

The engine reads each switch **once** at startup (or at a memoized first use),
so set it on the process, not mid-run. Nothing here needs a special build: the
default build honors every switch. `slag --help` prints the common engine
switches under an `environment variables` heading; this file is the full
reference and adds the test/tooling switches.

## Engine tuning

These choose which optimizations the JIT applies. They are the seams for A/B
measurement and the knobs an embedder can experiment with; `SLAG_OPT` and
`SLAG_TYPER` are the two a user is most likely to want.

| Variable | Default | Effect |
|---|---|---|
| `SLAG_OPT` | on | `=0` disables the **optimizing tier** (the SSA/opt path). The interpreter and the per-step Cranelift tier are unaffected — a body the optimizing tier declines keeps the per-step path either way. |
| `SLAG_TYPER` | on | `=0` disables the typer's **read guard** (`Op::GuardType` on a member/element read, which lets the arithmetic consuming it lower tag-free). Measured ~perf-neutral on the corpus (both directions within noise); kept on because it is free and can help hot numeric property loops. |
| `SLAG_INLINE` | **off** | non-`0` enables the whole-body **trial-inline splice** (`pass/inline.rs`). Needs `SLAG_FEEDBACK` for its call-site map. |
| `SLAG_INLINE_BUDGET` | `256` | a positive integer bounding the inliner's code-growth and self-recursion budget. |
| `SLAG_FEEDBACK` | **off** | non-`0` enables the **call-site feedback probe** (`runtime/src/feedback.rs`); unset allocates nothing and changes no behavior. |
| `SLAG_GUARD` | on | `=0` disables the read-guard **pruning pass** (`pass/guard.rs`). |
| `SLAG_MEM2REG` | on | `=0` disables top-level **frame-slot promotion** (`pass/mem2reg.rs`). |
| `SLAG_WIDEN` | on | `=0` disables `narrow`'s **effect widening**. |

## JIT diagnostics

All write to **stderr**. They are for debugging a workload, not for production.

| Variable | Effect |
|---|---|
| `JIT_DUMP_IR` | dump the optimizing tier's SSA IR for every lifted body (`--- IR ---`). |
| `JIT_DUMP_CLIF` | dump the Cranelift CLIF **and its disassembly** for every lowered body, plus the bail reasons: `opt bail:`, `jit bail:`, `jit skip:` (too large), `jit codegen error:`. |
| `JIT_DUMP_STEPS` | on a lift refusal, dump the blocking step names (`opt-blockers:`) and a shape census (`opt-shapes:`). |
| `JIT_HELPER_STATS` | the `slag` CLI prints per-helper call counts at exit (`helper <index> <count>`). |
| `JIT_LEAF_TRACE` | trace the first few leaf-probe rejections (`leaf-probe reject: <why>`). |
| `SLAG_JIT_DEOPT_PROBE=<step>` | force a deopt guard at that step in every eligible body — exercises the spill/resume path deterministically. |
| `SLAG_JIT_DEOPT_TRACE` | trace each deopt (`jit-deopt step=<n> depth=<n>`). |

A quick recipe for a body that runs slower than expected:

```sh
# Is the optimizing tier taking the body, and if not, why?
JIT_DUMP_CLIF=1 slag workload.js > /dev/null
# What does the body look like after the pass pipeline?
JIT_DUMP_IR=1 slag workload.js > /dev/null
```

## Test and tooling

Not part of the engine; they configure the in-repo runners.

| Variable | Effect |
|---|---|
| `SWEEP` | the test262 sweep area (default `all`). |
| `SWEEP_SAMPLE` | run only that many fixtures per top-level directory (a fast triage sample). |
| `WAST2JSON` | path to wabt's `wast2json`; forces the wabt converter in `wasmtest` instead of the in-process `wast` crate. |
| `WASM_EXCLUSIONS` | the wasm exclusion manifest path (default `crates/wasmtest/wasm-exclusions.txt`). |
| `WASM_JSAPI_TRACE`, `WASM_JSAPI_VERBOSE` | `wasmtest jsapi` diagnostics. |
| `SLAG_BIN` | the binary the `tools/corpus/*.js` runners drive (default `target/release/slag.exe`). |
| `CARGO`, `DENO_DIR`, `DENO_PIN`, `DENO_REPO` | `tools/build-deno.py` (the Rust toolchain, the checkout, the pinned revision, the repo URL). |

## See also: CLI flags

Several knobs are **flags**, not environment variables — the GC/stress controls
(`--gc-stress`, `--gc-verify`, `--nursery-stress`, `--nursery-threshold`,
`--gc-trace`) and the runner mode (`--jitless`, `--bench`, `--jit-bench`,
`--corpus`, `--dump-ast`, `--dump-tokens`, `--print-bytecode`). Run `slag --help`
for the current list.
