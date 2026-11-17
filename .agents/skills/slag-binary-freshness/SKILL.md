---
name: slag-binary-freshness
description: Verify a target/*.exe was built from the current source before trusting its output. Load before running any Slag binary (slag.exe, sweep.exe, leak.exe, examples) after source edits, git checkouts/stashes/resets, or when behavior contradicts the code you are reading (e.g. an error message that does not exist in crates/). Covers the xtask (`cargo run -p xtask -- build`) content-addressed binary cache, mapping each binary to its producing package, and mtime/cargo checks.
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

`cargo run -p xtask -- build` builds `slag`/`leak` (`cli`), `sweep` (`test262`)
and `wasmtest` (`wasmtest`) and caches the binaries under `target/build-cache`.
It lives in `crates/xtask`; the flags below are its own.

- The key is the content of the **paths the binaries are compiled from**, not
  the commit and not the whole tree: `git add -A -- <KEY_PATHS>` on a throwaway
  index (`target/.build-cache-index`) followed by `git write-tree`, hashed with
  the profile, `rustc -vV`, the checkout path, `RUSTFLAGS`, `RUSTC`,
  `CARGO_BUILD_TARGET`, and the test262 submodule's HEAD plus status hash.
  `KEY_PATHS` (`crates/xtask/src/main.rs`) is `crates/` plus the root
  `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` and `.cargo`.
  - **A note, skill, doc or probe edit does not change the key** — that is the
    point: `.notes/`, `.agents/`, `docs/`, `scratch/` and `tools/` are out, so
    writing up a measurement after it lands does not invalidate the binaries you
    just built and want as the A/B control.
  - The tree hash is therefore **not** `HEAD^{tree}`; it is the hash of a
    partial tree holding only those paths. A tree assembled by
    `git checkout <rev> -- crates/` still gets its own entry, which a commit
    hash could not do.
  - Deliberately still keyed: a crate's own prose and data files, because
    `crates/` is one entry and crates `include_str!` files beside their sources
    (`runtime/src/rlx.rs`, the `examples`). Narrowing by extension would trade a
    rare needless rebuild for a real stale-binary hazard.
  - `cargo test -p xtask` pins the allowlist to the workspace: a member outside
    `KEY_PATHS` fails the test rather than silently escaping the key.
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
  is missing an input and the tool needs fixing.
- `--no-cache` bypasses the cache. Entries are ~60 MB each; delete
  `target/build-cache` to reclaim the space. `SLAG_BUILD_CACHE` relocates it;
  keep the cache per-checkout, since binaries embed the building path.

## Never leave an artifact built from source you have since reverted

The cache key is the tree content, so a dirty entry is *correct for that content*.
The hazard is the fixed-name copy: the build task writes
`target/<profile>/{slag,sweep,leak,wasmtest}.exe`, and nothing rewrites them when
the tree moves. Build an experiment, revert it, and `target/release/sweep.exe`
keeps running the experiment — a gate run then reports the experiment's failures
against the reverted source. This has already happened: an experimental
`UpdateContextSlot` lift was built, reverted, and left in `target/release`; a
sweep afterwards reported `Iterator/zip/basic-longest.js` failing on a tree whose
source refuses that lift (`refuses_update_context_slot` pins it).

So, when abandoning an experiment:

1. Rebuild from the reverted tree: `cargo run -p xtask -- build`. If the reverted
   tree is a commit you already built, this is a **0.6 s hit**, not a rebuild —
   which is the whole point of the cache. (It is only instant if that commit's
   tree has an entry; see "build after the last edit" below.)
2. Delete its cache entry, but decide by the **tree**, not by `state dirty`. A
   `meta.txt` reads `state dirty` whenever the tree was not `HEAD` at build time
   — which includes a commit's own tree built before the commit object existed
   (`d17579b7…` is `d3c5cd43`'s tree and says `dirty`). Compare `tree` against
   `git log --format=%T` and delete only entries that match no commit, and only
   for experiments you are actually abandoning; an entry for a commit you may
   measure against later is the instrument, not waste.
3. Only then run a gate, and say which tree the binary came from. A green sweep
   from an entry whose tree matches no commit is not evidence about the source.

## Build after the last *source* edit

The cache keys on the keyed paths, so the build that lands an entry must come
**after** the last thing that changes one of them — including `cargo fmt` and the
pre-commit hook's `fmt --all`, which rewrite crate sources. A session that
builds, then formats, then commits leaves its own commit with no entry. Measured
over one session, before `KEY_PATHS` existed: of 10 builds, 3 matched a commit
and 7 matched nothing (5 from building before the tree was final — a later edit
or the final `cargo fmt` — and 2 from reverted experiments), with **2 of the 4
commits having no entry at all**: the two a later measurement wanted as controls.
The cost is exactly the rebuild you were trying to avoid, at the worst moment.

The order that keeps it: edit → `cargo fmt --all` → `cargo run -p xtask -- build`
→ commit. A commit that touches only notes, skills or docs does **not** change
the key, so it needs no rebuild; that is why `KEY_PATHS` is an allowlist rather
than the whole tree.

Conversely, a *legitimate* same-tree rebuild is cheap: a hit copies in well under
a second, so there is never a reason to skip a rebuild before reporting results.

Conversely, a *legitimate* same-tree rebuild is cheap: a hit copies in well under
a second, so there is never a reason to skip a rebuild before reporting results.

## The C++ facade: `slagc.lib` and the samples

`crates/v8/cpp` links `target/<profile>/slagc.lib` (the `slagc` package) into
`crates/v8/cpp/build/<Config>/{hello-world,shell,gin-shapes}.exe`.

- **`cargo check -p slagc` does not write the staticlib.** After editing
  `crates/slagc`, run `cargo build -p slagc` *and* rebuild the CMake target; a
  `check` alone leaves the exe linked against the previous ABI, and the symptom
  is a crash in a code path you did not change.
- The exe relinks only when CMake sees the `.lib` newer, so confirm with
  `ls -l --time-style=+%H:%M:%S target/debug/slagc.lib \
   crates/v8/cpp/build/Debug/gin-shapes.exe` before trusting a sample.

### The ABI struct mirror

Every `extern "C"` struct is declared twice: in
`crates/v8/cpp/include/slag_a0.h` and as a `#[repr(C)]` type in `crates/slagc`
(e.g. `SlagPropertyHandler`). Change both in the same edit. A field added or
removed on one side only shifts every later field, so the Rust side reads the
*next* field's value — a `state` holding `7.0`'s bits (`0x401C...`) is the
signature — and the crash lands at the first indirect call through the wrong
pointer, with no Rust-side message. Grep both files for the struct name when a
handler or callback struct changes.

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
