# Slag vs Brimstone

A standing cross-engine comparison. Brimstone (`github.com/Hans-Halverson/brimstone`)
is a bytecode interpreter with no JIT, so it is the yardstick for two questions
at once: is our *interpreter* competitive (`--jitless` vs `bs`), and what does
the JIT actually buy (`slag` vs `--jitless`), measured against a real engine
rather than against ourselves.

Point `BS` at a release build of its `bs` binary (it must be at least as new as
its sources — a stale `bs.exe` measures old code):

```
cargo build --release --bin bs          # in the brimstone checkout
tools/brimstone/compare.sh 5            # BS=/path/to/bs.exe to override
```

Each workload times itself (`Date.now` around the work) and prints
`<name> <ms> ...`, so process startup and parse are excluded. The harness runs
one process per (engine, workload), repeats `rounds` times with the engine order
rotated each round, and reports the minimum per cell. `jit/bs` and `njit/bs`
are ratios where below 1 means Slag is faster.

## Reading the suite

The suite mixes two shapes on purpose, because they are not the same
measurement in Slag:

- **Top-level loops** (`arith`, `array`, `forin`, `method`, `object`) — the loop
  is in the script body. A script that calls anything through a member
  expression (`console.log`, `Date.now`, `p.get()`) fails `analyze_script_scope`
  and runs **entirely interpreted**, so `jit` and `njit` are the same number.
- **Function loops** (`fn_arith`, `fn_array`, `fn_forin`, `fn_object`,
  `closures`, `capture`, `capture2`, `local`, `fib`, `sieve`, `string`) — the
  loop is inside a function body, which certifies independently of the script.
- `script_only` / `script_fn` / `script_call` isolate that one cause: the same
  10M-iteration loop with no call, with a call to a certified top-level
  function, and with `Date.now`/`console.log`.

Do not compare a `jit` column across those groups without reading the
`scripts`/`fn_` prefix.
