# The typer read guard miscompiled Deno's header path (fixed)

**Status: FIXED (2026-10-11).** The optimizing tier's typer read guard
(`SLAG_TYPER`, on by default since `eaaacae4`) miscompiled Deno's
`Headers`/`Response` construction: a `Headers` built from a record kept no
entries once its body was compiled, so `response.headers.get("etag")` answered
`null`. The guard is correct; the bug was a **deopt from a compiled caller's
certified-callee lane swallowed as a value**. Fixed in `certified_lane_inline`
and the guard is on by default again.

## Root cause

`certified_lane_inline` (`.notes/…`, `crates/runtime/src/jit.rs`) has two
routes: the **probe path** (`cached: Some(..)`, from `certified_call`) and the
**derived fallback** (`cached: None`, from `call_slow` and
`certified_construct_lane`). Every other machine-code lane refuses a
deopt-bearing callee, the caller's compiled code has no deopt path, so a
`DISPATCH_DEOPT` would surface as a bogus value. `certified_verdict` (the probe)
and `leaf_call_probe` both check `compiled.deopts` for exactly this reason.

The **derived fallback did not**. It consulted `lookup_info`, read
`info.entry`/`info.stack_usage`, and ran the callee's machine code, with no
`info.deopts` gate. When the callee's body then deopted, its `-4`
(`DISPATCH_DEOPT = u64::MAX - 3`) was returned as `Ok(Some(result))`, i.e. **as
if it were the call's return value**, straight back into the caller's compiled
frame.

Deno's `headerListFromHeaders` (`ext/fetch/20_headers.js:744`) is that callee:
its read guard (`is_double` on `headers[_headerList]`, an array) fires on every
call, so the body deopts every call. Called from the compiled `Headers.get`
through `call_slow`, the deopt sentinel came back as the header list, hence the
empty list. The `-4` never reached `run_jit_body`'s `DISPATCH_DEOPT` branch,
which is why the earlier probes saw "no deopt".

## Fix

One gate in the derived path, mirroring the probe path:

```rust
let info = unsafe { &*info_ptr };
if info.deopts {
    return Ok(None);
}
```

`Ok(None)` falls out to the interpreter funnel, which resumes the deopt through
`run_jit_body` (the designed path). This covers both `call_slow`'s fallback and
`certified_construct_lane` (same derived route). A post-hoc
`result == DISPATCH_DEOPT` check would be wrong, the callee's machine code has
already run, so re-running the whole body interpreted could repeat side effects;
the pre-check refuses before any of it runs.

Regression test:
`opt_tier_guarded_callee_deopt_from_a_compiled_caller_matches_the_interpreter`
(`crates/jit/src/lib.rs`), Deno's `headerListFromHeaders` shape, called from a
compiled `run`, compared against the interpreter.

## Gates (guard on by default)

`cargo test -p jit --lib` 357/0; `cargo test -p runtime --lib` 1042/0; clippy
`--workspace --all-targets -D warnings` clean; test262 `language`
23726/0/0/0/0 and `built-ins` 23820/0/1/0/0; Deno surface 29/29; `h_only.js`
(`new Headers({etag:"1"})` × 6) all `"1"`.

## How the earlier investigation was misled (kept for the record)

The bug looked like a miscompiled body because the swallowed deopt made the
DISPATCH_DEOPT branch never run:

- `SLAG_JIT_DEOPT_TRACE=1` and an unconditional print in `run_jit_body`'s
  `DISPATCH_DEOPT` branch were both **silent**, because the deopt never got
  there (it was returned as a value). This is a *symptom*, not evidence the guard
  passed.
- The culprit body's CLIF differs from the guard-off build by the guard's blocks
  **only** (a normalized diff), and the emitted machine code for the guard and
  its continuation is correct, the guard checks the value `cont` stores. There
  is no CLIF bug to find.
- Forcing the guard's condition true (so Cranelift drops the `deopt` block)
  "fixed" it, because that removes the deopt that was being swallowed. Bare
  deopt and lazy seal did not change it, for the same reason: the failure was
  never in the emitted guard code.
- Only `SLAG_TYPER` mattered (`SLAG_GUARD`, `SLAG_MEM2REG`, `SLAG_WIDEN`,
  `SLAG_INLINE` were all inert), the read guard is what can deopt.
- The first call was right and every later call wrong because the first call ran
  interpreted; later calls reached the compiled caller's `call_slow` lane.

The CLI mimics never reproduced because a caller has to be **compiled** for the
call to reach `call_slow`'s certified lane rather than `run_jit_body`.
