# Fuzz targets

`cargo fuzz` targets for the three front ends that read untrusted input before
anything executes: the JavaScript parser (UTF-8 and UTF-16), the WebAssembly
binary decoder plus validator, and the RegExp compiler plus matcher. They cover
the defect classes of M-9 — a panic, abort, or hang on hostile input — for the
stages a host reaches with no capability beyond "parse this".

This directory is its own workspace (`fuzz/Cargo.toml` has an empty
`[workspace]` table) because the sanitizer build needs nightly and its own
profile. The root workspace's lint and test gates do not see it.

## Running

```
cargo install cargo-fuzz           # once
cargo +nightly fuzz run parse_js -- -seed_inputs=fuzz/seeds/parse_js -max_total_time=60
```

The targets are `parse_js`, `parse_js_utf16`, `wasm_decode`, and `regexp`.
Crashes land in `fuzz/artifacts/<target>/`; reproduce one with
`cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<file>`.

`fuzz/seeds/` holds a handful of inputs chosen so each target reaches its
decoder on the first iteration instead of having to discover an 8-byte module
header or a valid program by chance. Pass them with `-seed_inputs=` (read-only)
rather than as a corpus directory, which libFuzzer would keep writing into.

## In CI

The `fuzz` job runs only from a manual `workflow_dispatch` with its input set,
like the Miri job, and bounds each target with `-max_total_time`, so no run can
stall a build. A nightly toolchain and `cargo-fuzz` are installed in the job;
nothing in the normal build depends on either.

## Scope

Not covered, deliberately or for now: the runtime itself (the collector needs
the native stack, and an `Agent` is not `Send`), the snapshot decoder (covered
by the deterministic bit-flip sweep instead), and the Cranelift path (its input
is a validated module, which the `wasm_decode` target already produces).
