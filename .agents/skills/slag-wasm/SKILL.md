---
name: slag-wasm
description: Load when working on Slag's WebAssembly engine — the decoder/validator (crates/wasm/src/binary.rs, valid.rs), the interpreter (exec.rs), the Cranelift backend, the JS-API layer (crates/runtime/src/builtins/wasm.rs), or the wasm sweeps (`wasmtest run` / `wasmtest jsapi`). Covers the load-bearing `Unsupported` vs `Malformed` classification contract, the sweep shape and exclusions, and the shared-memory buffer sealing rule. For test262 sweeps load slag-conformance instead.
---

# Slag WebAssembly engine

Traps for `crates/wasm` (decode, validate, execute, compile), the JS-API
layer, and the wasm conformance runner. State and design live in
`.notes/wasm-plan.md`, `.notes/wasm-analysis.md`, and `.notes/wasm-depth.md`
— this skill is only the traps.

## Sweeping

- The corpus is the pinned `waspec` submodule. Initialise it first
  (`git submodule update --init waspec`); with it empty, every documented
  number looks irreproducible and no fixture exists on disk.
- Sweep the suites **separately**: `wasmtest run waspec/test/core/*.wast`,
  then each proposal directory (`simd`, `relaxed-simd`, `bulk-memory`,
  `exceptions`, `gc`, `memory64`, `multi-memory`). One invocation spanning
  `core/` and `multi-memory/` refuses to run: `memory_grow.wast` exists in
  both and they share the runner's cache key.
- `wasmtest` takes paths only — `--timeout` is not one of its flags.
- The per-suite totals must match `README.md`'s table (core **64,594 / 0**,
  JS-API **1,001 / 0**); a mismatch means a stale binary or a real
  regression, not a rounding difference.
- Non-runnable files are the written taxonomy in
  `crates/wasmtest/wasm-exclusions.txt` (printed as `skip`), not silent
  misses.

## Trap 1 — `Unsupported` vs `Malformed` is load-bearing

`binary.rs`'s `Error` splits "the bytes are structurally wrong"
(`Malformed`) from "a feature this engine does not decode yet"
(`Unsupported`), and the runner's verdict mapping is **arm-dependent**:

| command | `Malformed` | `Unsupported` |
|---|---|---|
| `assert_malformed` | pass | pending |
| `assert_invalid` | **fail** ("expected invalid, decoder says malformed") | pending |
| plain `module` | fail | pending |

So the two directions both lie: reporting `Unsupported` for an encoding the
pinned spec never defines parks a fixture as pending (and "0 pendings" is a
gate), while reporting `Malformed` for a real later-cut feature turns a pass
into a fail. Before flipping an arm, check the encoding is genuinely outside
the pinned spec — e.g. every SIMD subopcode is in `simd::sig` (relaxed SIMD
included, `0x100..=0x113`), so an unrecognised `0xfd` subopcode is malformed
rather than unsupported.

## Trap 2 — the decoder is strict end to end

- `decode_body` must consume the declared body size exactly; trailing bytes
  are malformed. The section-level `pos != payload.len()` check does not
  cover the body.
- Never `Vec::with_capacity(untrusted_count)` — push incrementally, as
  `read_valtype_vec` and the `MAX_LOCALS` cap already do.

The corpus does **not** exercise reserved-encoding classification or the
body-size check (0 pendings both before and after them), so unit tests in
`binary.rs`'s test module are the only coverage a change there gets — add
them with the fix.

## Trap 3 — shared-memory buffer sealing is wasm-path-only

`WebAssembly.Memory({shared:true}).buffer` must report `Object.isFrozen`
true and not be extensible (V8 reports the same). Seal it in
`builtins/wasm.rs::shared_memory_buffer`.

Do **not** move that seal into
`array_buffer::shared_array_buffer_from_block`: the worker paths
(`workers.rs`, the test262 harness) also wrap blocks there and hand them to
JS as ordinary SharedArrayBuffers, and making all SharedArrayBuffers
non-extensible breaks test262's SAB species fixtures
(`built-ins/SharedArrayBuffer/prototype/slice/species-*.js` assign
`constructor` on an instance, which throws in strict mode). V8 keeps the
asymmetry: a plain `new SharedArrayBuffer(8)` is extensible, the wasm
shared-memory buffer is sealed.

When two corpora appear to conflict, ask a real engine (`node -e "…"`)
before choosing a side — the resolution is often scoping, not either/or.
And treat a test whose assertion contradicts the comment directly above it
as suspect: the wasm test here asserted
`if (Object.isFrozen(sab) || !Object.isExtensible(sab)) return false` under
a comment that already said "a frozen SharedArrayBuffer".

## Validation loop

- `cargo test -p wasm`, `cargo test -p runtime --lib`.
- `cargo clippy --workspace --all-targets -- -D warnings`.
- `wasmtest check docs/slag.wasm` — a cheap end-to-end decode + validate
  gate over the real 7 MB compiler output.
- `cargo test -p wasmtest` — runs `tests/decoder_classification.rs` over
  `fixtures/decoder-classification.wast`. This is the *only* place the
  "0 pendings" claim is enforced: the runner exits 0 on a pending, so a
  regression that parks a reserved encoding as `Unsupported` is invisible to
  the sweep and visible only here. Adding a fixture case means bumping the
  pinned pass count in that test.
- `cargo run -q -p wasmtest -- run …` and `-- jsapi waspec/test/js-api`.
  `WASM_JSAPI_VERBOSE=1` prints the failing-test messages.
- When the change touches the buffer machinery, cross-check the test262
  clusters too: `target/release/sweep.exe built-ins --filter
  'SharedArrayBuffer*'` (and `ArrayBuffer*`, `Atomics*`) at
  `--jobs 8 --batch 32 --timeout 15 --recheck-timeout 15`.

## Relationship to the other skills

- `slag-conformance` — test262 sweeps and the failure-triage loop.
- `slag-binary-freshness` — every `target/*/wasmtest` claim depends on it.
- `git-commit-messages` — commit message format.
