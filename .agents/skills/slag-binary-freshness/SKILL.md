---
name: slag-binary-freshness
description: Verify a target/*.exe was built from the current source before trusting its output. Load before running any Slag binary (slag.exe, sweep.exe, leak.exe, examples) after source edits, git checkouts/stashes/resets, or when behavior contradicts the code you are reading (e.g. an error message that does not exist in crates/). Covers the tools/build.sh content-addressed binary cache, mapping each binary to its producing package, and mtime/cargo checks.
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

## Use the build cache, not a bare cargo build

`tools/build.sh` builds `slag`/`leak` (`cli`), `sweep` (`test262`) and
`wasmtest` (`wasmtest`) and caches the binaries under `target/build-cache`.

- The key is the **content of the working tree**, not the commit: `git add -A`
  on a throwaway index (`target/.build-cache-index`) followed by `git write-tree`,
  hashed together with the profile, `rustc -vV`, the checkout path, `RUSTFLAGS`,
  `RUSTC`, `CARGO_BUILD_TARGET`, and the test262 submodule's HEAD plus status
  hash. On a clean checkout the tree hash equals `git rev-parse HEAD^{tree}`; a
  tree assembled by `git checkout <rev> -- crates/` gets its own entry, which a
  commit hash could not do.
- Output is one line: `build-cache: hit <tree16>/ <profile>, HEAD <sha> clean|dirty`
  or `miss`. A hit copies the stored binaries into `target/<profile>/` in well
  under a second; a miss runs cargo and stores the result.
- `--print-key` prints `tree`, `config`, `entry` and HEAD without building — use
  it to prove whether an edit actually changed the key.
- `-p cli` (or `-p test262`, `-p wasmtest`, repeatable) limits the build. The
  packages resolve `runtime` with different feature sets, so they are built one
  at a time; do not merge them into one cargo invocation, or feature unification
  will change the binaries.
- A hit is content-equivalent to a fresh build, so it is safe to trust. If a hit
  still contradicts the source, inspect `target/build-cache/<entry>/meta.txt`
  (tree, HEAD, rustc, date) and rebuild with `--force`; if that fixes it, the key
  is missing an input and the script needs fixing.
- `--no-cache` bypasses the cache. Entries are ~60 MB each; delete
  `target/build-cache` to reclaim the space. `SLAG_BUILD_CACHE` relocates it;
  keep the cache per-checkout, since binaries embed the building path.

## Never leave an artifact built from source you have since reverted

The cache key is the tree content, so a dirty entry is *correct for that content*.
The hazard is the fixed-name copy: `tools/build.sh` writes
`target/<profile>/{slag,sweep,leak,wasmtest}.exe`, and nothing rewrites them when
the tree moves. Build an experiment, revert it, and `target/release/sweep.exe`
keeps running the experiment — a gate run then reports the experiment's failures
against the reverted source. This has already happened: an experimental
`UpdateContextSlot` lift was built, reverted, and left in `target/release`; a
sweep afterwards reported `Iterator/zip/basic-longest.js` failing on a tree whose
source refuses that lift (`refuses_update_context_slot` pins it).

So, when abandoning an experiment:

1. Delete the binaries built from it — rebuild from the reverted tree
   (`./tools/build.sh`) rather than leaving the old copies.
2. Delete its cache entry: `target/build-cache/<entry>` holds the artifacts and a
   `meta.txt` whose `state dirty` (or a `tree`/`head` that no longer matches) is
   the tell. Leaving it wastes ~60 MB and invites a future `--force` mix-up.
3. Only then run a gate, and say which tree the binary came from. A green sweep
   from a `state dirty` entry is not evidence about the committed source.

Conversely, a *legitimate* same-tree rebuild is cheap: a hit copies in well under
a second, so there is never a reason to skip a rebuild before reporting results.

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
