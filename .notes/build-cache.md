# Build cache: reuse binaries across tree states

**Status: implemented** as `tools/build.sh` (2026-10-20). Measured on a Windows
x86-64 host with the release profile from the workspace `Cargo.toml`
(`lto = "thin"`, `codegen-units = 1`).

## The problem

A release rebuild is minutes, and the work repeated most often is a tree the
machine has already built:

- a bisect returns to a known-good revision to A/B it;
- an experiment is reverted, then re-applied;
- a docs-only edit invalidates cargo's mtime checks for a binary that is
  byte-identical in substance.

Cargo alone cannot help: it keeps one artifact per crate per feature
resolution, so checking out an older `crates/` and back rebuilds everything that
moved. The binaries themselves are small enough to keep.

## Key: the working tree, not the commit

Bisects here replace individual paths (`git checkout <rev> -- crates/`), so the
compiled tree frequently matches **no commit**. A commit hash would be wrong in
both directions: a miss for a tree that is exactly a previous build, and a hit
for a dirty tree whose sources differ from the commit.

`tools/build.sh` snapshots the working tree instead:

```sh
GIT_INDEX_FILE=target/.build-cache-index git add -A
GIT_INDEX_FILE=target/.build-cache-index git write-tree
```

The throwaway index leaves the real one untouched. The resulting hash is
content-addressed and, on a clean checkout, exactly `git rev-parse HEAD^{tree}`.
Ignored paths (`target/`, `scratch/`, `/v8/`, `/deno/`, `docs/slag.wasm`) stay
out of the key; non-ignored untracked files go in, which is the conservative
direction.

The key is `tree hash + config hash`, where the config hashes `rustc -vV`, the
profile name, the checkout path (binaries embed it in source paths and debug
info), `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS`, `RUSTC`,
`CARGO_BUILD_TARGET`, and the test262 submodule's HEAD plus a hash of its
`status --porcelain` (the `unicode` build script reads its fixtures, and
`git add -A` records only a gitlink for it).

Entry: `target/build-cache/<tree:16>-<config:8>/`, override with
`SLAG_BUILD_CACHE`. `meta.txt` records the full tree and config hashes, the HEAD
commit and its subject-free short form, clean/dirty state, profile, rustc, build
timestamp, and the binaries present.

## Behaviour

| case | result | measured |
| --- | --- | --- |
| hit (all requested binaries present) | `cp` into `target/<profile>/` | **0.63-0.74 s** |
| cold miss, all packages | three cargo builds, then store | **8 m 8 s** |
| cold miss, `-p cli` | one cargo build, then store | **2 m 42 s** |
| miss caused by a non-Rust edit | cargo proves freshness, store | **2.5 s** |
| key after a path-level checkout | different entry | verified |

The 8 minutes are three `runtime` builds, not one repeated by mistake: `cli`,
`test262` and `wasmtest` resolve `runtime` with different feature sets, so cargo
compiles it once per package. They are built as separate `cargo build -p`
invocations on purpose, selecting all three in one invocation unifies their
features and changes the binaries from what CI publishes.

Entries are partial by design. Building `-p cli` at tree T stores `slag` and
`leak`; a later `tools/build.sh` at the same T misses (it wants `sweep` and
`wasmtest` too), cargo's fingerprints are still valid so nothing recompiles, and
the merge stores the union. A hit requires every requested binary, never a
subset.

A build that changes the tree (a build script writing a tracked file, a cargo
lock update) is detected: the key is recomputed and the binaries are stored
under the post-build tree, with a line saying so.

## What it does not do

- It does not cache cargo's intermediate artifacts; only the final binaries.
  Revisiting a tree builds nothing, but a tree that was never built still pays
  the full compile.
- It does not prune. An entry is ~58 MB (four binaries); `rm -rf
  target/build-cache` is the cleanup. `cargo clean` also removes it, since the
  cache lives under `target/`.
- It does not run under `cargo`; callers must use the script (or copy binaries
  out of an entry) to get the reuse. CI builds each package in its own job and
  does not use it.

## Verification performed

1. Clean tree: `--print-key` tree hash equals `HEAD^{tree}`.
2. Cache miss → store → immediate re-run → hit in 0.66 s.
3. Deleted `target/release/slag.exe` and `sweep.exe`; a hit restored both, and
   `slag --version` / `sweep --help` ran.
4. `git checkout HEAD~1 -- crates/runtime/src/ir.rs` produced a different key
   with no commit change; restoring the file returned to the original key and a
   hit. The working tree was confirmed clean afterwards.
5. `-p nope` exits 1 with the package list.
