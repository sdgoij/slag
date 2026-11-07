#!/usr/bin/env bash
# Build both mutually-exclusive C-skin variants of the quickjs facade and keep
# their artifacts separate. `ng` and `bellard` each produce a `libquickjs`
# staticlib (and a shared library) under the same target dir, so the second
# build would overwrite the first; copy each build's artifacts to
# `target/quickjs/<feature>/` before moving on.
set -euo pipefail

cd "$(dirname "$0")/.."

for feature in ng bellard; do
    cargo build --locked -p quickjs --features "$feature" --release
    out="target/quickjs/$feature"
    mkdir -p "$out"
    # The staticlib (libquickjs.a, or quickjs.lib on MSVC) and the shared library
    # (libquickjs.so / libquickjs.dylib / quickjs.dll), whatever cargo named them
    # on this platform.
    find target/release -maxdepth 1 \
        \( -name 'libquickjs.a' -o -name 'quickjs.lib' \
           -o -name 'libquickjs.so' -o -name 'libquickjs.dylib' \
           -o -name 'quickjs.dll' -o -name 'libquickjs.dll' \) \
        -exec cp -f {} "$out/" \;
done
