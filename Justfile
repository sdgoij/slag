# Task entry points for building, testing and developing.
#
# `just` on its own lists everything. The recipes call the tracked tools rather
# than reimplementing them: `cargo run -p xtask -- build` owns the
# content-addressed binary cache, tools/build-deno.py owns the host build, and
# .github/workflows/ci.yml is the source of truth for what a gate is.

set shell := ["sh", "-cu"]

# The recipes are POSIX sh; the cache build is `crates/xtask`. The four QuickJS
# scripts are still bash, and on Windows `bash` on PATH is the WSL launcher in
# System32, which cannot run them, so those calls name Git's bash outright.
# SLAG_BASH overrides the location when Git is installed somewhere else.
bash_bin := if os() == "windows" {
    env("SLAG_BASH", "C:/Program Files/Git/bin/bash.exe")
} else {
    "bash"
}

# Local sweep deadlines. The repository rule is 15s and CI raises it to 30/60 to
# absorb a slower runner VM; locally, something that overruns 15s is a real
# result rather than a mis-timed deadline.
sweep_timeout := "15"

# List the recipes.
default:
    @just --list

# Point git at the tracked hooks (the pre-commit hook runs what `just lint` runs).
setup:
    git submodule update --init
    git config core.hooksPath .githooks
    rustup show

# cargo-deny is not a subcommand cargo ships with.
setup-deny:
    cargo install cargo-deny --locked

# Apply rustfmt to the workspace and to the separate fuzz workspace.
fmt:
    cargo fmt --all
    cargo fmt --manifest-path fuzz/Cargo.toml

# Formatting only, as the Format and lints job checks it.
fmt-check:
    cargo fmt --all -- --check
    cargo fmt --manifest-path fuzz/Cargo.toml -- --check

# Clippy for the workspace and, separately, for the fuzz workspace.
clippy:
    cargo clippy --locked --workspace --all-targets -- -D warnings
    cargo clippy --locked --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings

# Licence, advisory, yanked-crate and source policy, from deny.toml.
deny:
    cargo deny check

# RustSec advisories against Cargo.lock, as the Dependency audit job runs them.
audit:
    cargo audit

# Everything the pre-commit hook runs, in the hook's order.
lint: fmt-check clippy deny
    @echo "lint: ok"

# The workspace suite, every crate including jsc. `just test -p jit` narrows it.
test *args:
    cargo test --locked --workspace {{args}}

# The wasm-free shapes the workspace build never compiles.
test-wasm-free:
    cargo test --locked -p runtime --no-default-features --lib
    cargo check --locked -p cli --no-default-features --features jit

# Release binaries through the tree-keyed cache: `just build -p cli --debug`.
build *args:
    cargo run --quiet -p xtask -- build {{args}}

# The CLI binaries (slag and leak).
build-cli:
    cargo run --quiet -p xtask -- build -p cli

# The test262 runner (sweep).
build-sweep:
    cargo run --quiet -p xtask -- build -p test262

# The WebAssembly runner (wasmtest).
build-wasmtest:
    cargo run --quiet -p xtask -- build -p wasmtest

# The cache key and toolchain this tree would build under.
cache-key:
    cargo run --quiet -p xtask -- build --print-key

# JIT against interpreter on the CLI micro-suite.
bench:
    cargo run --quiet -p xtask -- build -p cli
    target/release/slag --jit-bench

# Cross-engine corpus against node; see tools/corpus/README.md.
corpus *args:
    node tools/corpus/bench.js {{args}}

# Run a script on the release binary: `just run scratch/foo.js`.
run *args:
    cargo run --quiet -p xtask -- build -p cli
    target/release/slag {{args}}

# test262 area sweep: `just sweep language`, or built-ins, annexB, intl402.
sweep area="language" *args:
    cargo run --quiet -p xtask -- build -p test262
    target/release/sweep {{area}} --batch 32 --timeout {{sweep_timeout}} --recheck-timeout {{sweep_timeout}} {{args}}

# The GC-stress gate for per-allocation collection, matching the gc-stress job.
gc-stress:
    cargo run --quiet -p xtask -- build -p test262
    target/release/sweep built-ins --filter "*Array*" --sample 4 --gc-stress --gc-verify --timeout {{sweep_timeout}} --recheck-timeout {{sweep_timeout}}
    target/release/sweep language --filter "*array*" --sample 4 --gc-stress --gc-verify --timeout {{sweep_timeout}} --recheck-timeout {{sweep_timeout}}

# WebAssembly conformance, the same way the wasm CI job sweeps it. One suite per
# invocation: duplicate .wast names across directories collide on the runner's
# cache key. A single line so the loop's variables stay in one shell, and it keeps
# CI's "every suite runs, fail at the end" behaviour.
wasm-sweep:
    cargo run --quiet -p xtask -- build -p wasmtest && { s=0; target/release/wasmtest run --strict waspec/test/core/*.wast || s=1; for suite in exceptions simd relaxed-simd bulk-memory memory64 multi-memory gc; do target/release/wasmtest run --strict "waspec/test/core/$suite" || s=1; done; target/release/wasmtest jsapi waspec/test/js-api || s=1; exit $s; }

# Build the browser demo module into docs/slag.wasm (gitignored).
demo:
    rustup target add wasm32-unknown-unknown
    cargo build --locked --release -p slag --example wasm_binding --target wasm32-unknown-unknown
    cp target/wasm32-unknown-unknown/release/examples/wasm_binding.wasm docs/slag.wasm

# Deno built against crates/v8, then both embedding harnesses.
deno *args:
    python tools/build-deno.py {{args}}
    bin=./deno/target/release/deno; [ -x "$bin" ] || bin="$bin.exe"; "$bin" run -A tools/deno_smoke.js; "$bin" run -A tools/deno_surface.js

# celld built against crates/v8.
celld:
    python tools/build-celld.py

# The QuickJS facade: both C skins, the link test, and the header and symbol drift checks.
quickjs:
    "{{bash_bin}}" tools/build-quickjs.sh
    "{{bash_bin}}" tools/link-test-quickjs.sh
    "{{bash_bin}}" tools/check-quickjs-headers.sh
    "{{bash_bin}}" tools/check-quickjs-surface.sh

# Miri over the front end; minutes per crate under the interpreter, so it is manual in CI too.
miri:
    cargo +nightly miri test -p lexer --lib
    cargo +nightly miri test -p syntax --lib

# One target for a bounded time (`just fuzz regexp 60`), corpus from fuzz/seeds.
fuzz target="parse_js" seconds="300":
    cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}} -max_len=65536 -rss_limit_mb=4096 -seed_inputs=fuzz/seeds/{{target}}

# Build artifacts only; the binary cache under target/build-cache survives.
clean:
    cargo clean

# Drop the cached binaries too, at roughly 60 MB per entry.
clean-cache:
    rm -rf target/build-cache
