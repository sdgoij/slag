#!/usr/bin/env bash
# Fail if a vendored QuickJS header drifts from its pinned checksum. The
# headers under crates/quickjs/c/include/ are vendored (and the Bellard one is
# patched for MSVC + inline-refcount removal), so they must not change without
# an intentional re-vendor that updates the pinned hashes here.
set -euo pipefail

cd "$(dirname "$0")/.."

fail=0

check() {
    local file="$1" expected="$2" actual
    actual="$(sha256sum "$file" | awk '{print $1}')"
    if [ "$actual" != "$expected" ]; then
        echo "header drift: $file" >&2
        echo "  expected $expected" >&2
        echo "  actual   $actual" >&2
        fail=1
    fi
}

check crates/quickjs/c/include/ng/quickjs.h \
    6d3879c856551653a1c64fabe52d6d07dc84e97bac3df45848df626702fc30de
check crates/quickjs/c/include/bellard/quickjs.h \
    6904cf9ef0aa0b8ff4e9f32f8fa0e19a3971ebb1433c02a7854c8ea81083ec26

exit "$fail"
