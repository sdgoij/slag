---
name: slag-binary-freshness
description: Verify a target/*.exe was built from the current source before trusting its output. Load before running any Slag binary (slag.exe, sweep.exe, leak.exe, examples) after source edits, git checkouts/stashes/resets, or when behavior contradicts the code you are reading (e.g. an error message that does not exist in crates/). Covers mapping each binary to its producing package and mtime/cargo checks.
---

# Slag binary freshness

A built `.exe` under `target/` only reflects the sources that were compiled when it
was linked. After another agent edits sources, or a `git checkout`/`stash`/`reset`
moves the tree, the on-disk binary can silently run OLD code — a "fix" you verify
may not be in the binary at all, and behavior can contradict the source you read.

## Map each binary to its producing package

Cargo builds per package; rebuilding a dependency does NOT relink the binaries
that embed it. `cargo build -p <lib>` is not enough when the `.exe` belongs to a
different package. Known mapping (verify with `cargo metadata --no-deps` when in
doubt):

| binary | producing package | rebuild |
|---|---|---|
| `target/{debug,release}/slag.exe` | `cli` (`[[bin]] name = "slag"`) | `cargo build -p cli` (+ `--release`) |
| `target/{debug,release}/sweep.exe` | `test262` (bin `sweep`) | `cargo build --release -p test262` |
| `target/{debug,release}/leak.exe` | `cli` (bin `leak`) | `cargo build -p cli` (+ `--release`) |
| `target/{debug,release}/publish.exe` | `cli` (bin `publish`) | `cargo build -p cli` (+ `--release`) |
| `crates/slag/examples/*` | `slag` (library) | `cargo build -p slag --examples` |

The package `slag` is a LIBRARY (`crates/slag`); it does not produce `slag.exe`.
Do not assume `cargo build -p slag` rebuilt the CLI.

## Freshness checks (run before trusting output)

1. Confirm the build is up to date and note whether it relinks:
   `cargo build -p cli 2>&1` — a bare `Finished` means the exe already matched the
   current sources; any `Compiling`/`Linking` means the previous exe was stale.
   (After a git operation that moved sources, always rebuild the producing
   package; the debug and release profiles are separate — the conformance sweep
   needs the RELEASE binary: `cargo build --release -p test262`.)
2. mtime spot-check (Git Bash): the exe must be newer than the newest source it
   compiles:
   `ls -l --time-style=full-iso target/debug/slag.exe`
   `find crates/runtime/src crates/cli/src crates/crux/src -name '*.rs' -newer target/debug/slag.exe | head`

## Symptoms of a stale binary

- Behavior contradicts the tree: e.g. a `RangeError` message string that exists
  nowhere under `crates/` (grep it) proves the exe predates a revert.
- A conformance/sweep result that does not match what the current code should do
  (see the `slag-conformance` skill trap: stale release sweep binaries silently
  measure old code).
- Your edit appears to have no effect when re-run.

When behavior does not match the code you are reading, rebuild the producing
package first and re-run before debugging further.
