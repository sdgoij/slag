# Wasm-in-wasm depth experiment (2026-09-06)

Experiment: how many layers of "wasm running wasm" can Slag do? Chain tested:

```
native slag.exe  (host, L0)
  └─ wasm engine interprets  wasm_binding.wasm  (a full Slag, "E1")
       └─ E1's own WebAssembly engine interprets  a small workload module
```

## Setup (how to reproduce)

Fresh builds:

```sh
cargo build --release -p cli
cargo build -p slag --example wasm_binding --target wasm32-unknown-unknown --release
```

Nested-engine bytes: `target/wasm32-unknown-unknown/release/examples/wasm_binding.wasm`
(7,546,346 bytes; 21 env imports, all functions: `slag_host_console`, `slag_host_has_dom`,
`slag_host_now_ms`, `slag_host_now_monotonic_ms`, and the `slag_host_dom_*`/storage/user
bridge). Native driver JS reads the binary as a hex text file (the CLI's `fs.readFileSync`
returns a *lossy string*, so raw bytes must be hex-encoded; the engine's JS has no
`atob`/`TextEncoder`/`fetch` and cannot view its own linear memory).

Driver recipe (harness kept in `tools/depth1.js`, `tools/depth4.js`, `tools/depth5.js` —
untracked by design):
1. Eval a driver in `slag.exe` that hex-decodes the engine binary, then
   `new WebAssembly.Module(bytes)` + `new WebAssembly.Instance(mod, env)`.
2. Provide env: `slag_host_has_dom = () => 0`, now → `Date.now()`, console → read text out of
   the child's `exports.memory` and `console.log` it, everything else `() => 0`.
3. Write a script into the child via `slag_alloc` + a `Uint8Array` view over
   `instance.exports.memory`, call `slag_eval`, read `slag_result_ptr/len/error`.

## Results

| Layer | Runs | Result |
|---|---|---|
| L1 | full Slag engine under native wasm engine | `Module` decode 113 ms, `Instance` 94 ms, engine boots **~11 s** (interpreted), `1+2` → `"3"`, `typeof WebAssembly` = `"object"` |
| L2 | workload module under E1's own engine | correct: `fib(10)=55`, `work(1000,10)=1769242148`, `hash(1000)=1681798947`, `lcg(1000n)=902429759771004424` |

So depth 2 (native → Slag-in-wasm → module) is real and correct. The browser demo's
"sandbox" is a *second, side-by-side* V8-instantiated module, not this nesting.

Cost structure: per wasm-interpretation layer ≈ **60–100×** slower than native (E1 boot
~11 s vs ~100–200 ms native). Compounding makes a second engine layer (E2) boot in
minutes and a third in hours. Byte hand-off to inner layers requires decoding the engine
binary in-band (E1's JS hex-decode of 7.5 MB did not finish in 30 min).

## Bug leads (nested execution only — NOT reproducible natively)

Two anomalies appeared only when the engine itself runs interpreted (inside E1).
`scratch/multi.js` runs the same instance patterns natively and passes.

### Anomaly 1 — later instance's exports become no-ops

Inside E1's JS: **create instance A, call an export of A, create instance B, then call
an export of B.** B's `i32`-returning exports return `undefined` immediately (body never
executes); B's `i64` export throws `TypeError: Cannot read properties of null`.

Working pattern (used in the L2 compute proof): instantiate a module and exercise it
*before* creating any further instance. Creating A, exercising A, creating B, then
exercising A again also works — only exercising the *newest* instance after a
create/call/create sequence misbehaves.

Smells like instance/store state: exports of the second instance resolving against a
stale or null store/instance pointer (i32 path silently defaults, i64 path derefs null).

### Anomaly 2 — sustained depth-2 compute never returns

Inside E1, running `fib(24)`, `work(100000,1000)`, `hash(100000)`, `lcg(200000n)` on one
bench-module instance produced no output within 15 minutes (killed). Small calls
(`fib(8)`, `work(1000)`, …) complete in seconds, so this is either a pathological
slowdown with workload size or a genuine hang in the engine-under-interpretation path.
Suspicion: something quadratic in nested dispatch, GC thrash under the much slower
outer interpreter, or the same instance-state corruption as Anomaly 1.

## What would unlock another level

1. Faster wasm-interpreter dispatch (per-layer 60–100× is mostly interpreter overhead).
2. A host export letting an engine's JS read/write its own linear memory, so a parent can
   write the child engine's binary into the child's memory (removes the 15 MB hex decode).
3. Fixing Anomalies 1–2 (likely instance/store bookkeeping or GC rooting that only
   triggers when the wasm engine itself is interpreted).

Harness: `tools/depth1.js` (+ `scratch/e1.hex`) = L1 proof, `tools/depth4.js` =
working L2 compute, `tools/depth5.js` = Anomaly 2, `scratch/multi.js` = native
negative control. The depth scripts are untracked in `tools/`; `e1.hex` (hex of the
7.5 MB engine binary) and `bench.dec` are scratch-only — regenerate `e1.hex` from the
target binary with `python -c "open('e1.hex','w').write(open('<wasm>','rb').read().hex())"`.
`scratch/` is gitignored; the depth drivers in `tools/` are intentionally left
uncommitted.
