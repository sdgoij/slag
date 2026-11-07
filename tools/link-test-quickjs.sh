#!/usr/bin/env bash
# Compile a tiny C host and link it against the facade's staticlib and cdylib,
# proving both artifacts are consumable from C. Each feature is built and
# linked in turn; the host evaluates `1 + 2` and the script asserts it prints
# `3`. Run on a Windows (MSVC) host with clang on PATH, or on Linux (CI) with
# cc/gcc.
#
# Usage: tools/link-test-quickjs.sh [ng|bellard ...]
set -euo pipefail

cd "$(dirname "$0")/.."

features=("$@")
if [ "${#features[@]}" -eq 0 ]; then
    features=(ng bellard)
fi

if command -v clang >/dev/null 2>&1; then
    CC=clang
elif command -v cc >/dev/null 2>&1; then
    CC=cc
elif command -v gcc >/dev/null 2>&1; then
    CC=gcc
else
    echo "link-test: no C compiler (clang/cc/gcc) found" >&2
    exit 1
fi

case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN* | Windows)
        is_windows=1
        ;;
    *)
        is_windows=0
        ;;
esac

# Artifact names follow the host toolchain, not the crate name.
if [ "$is_windows" = "1" ]; then
    staticlib="quickjs.lib"
    dll_import="quickjs.dll.lib"
    shim="qjs_shim.lib"
    system_libs=(-lws2_32 -lntdll -ladvapi32 -lbcrypt -luserenv -lshell32 \
        -lole32 -loleaut32 -luuid -lpsapi -ldbghelp)
else
    staticlib="libquickjs.a"
    shim="libqjs_shim.a"
    system_libs=(-lpthread -ldl -lm)
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

assert_output() {
    local file="$1" label="$2"
    local value
    value="$(tr -d '\r' < "$file")"
    if [ "$value" != "3" ]; then
        echo "link-test: $label produced '$value', expected '3'" >&2
        exit 1
    fi
}

for feature in "${features[@]}"; do
    cargo build --locked -p quickjs --features "$feature" --release

    out="target/release"
    header="crates/quickjs/c/include/$feature"
    src="crates/quickjs/c/link-test/hello.c"

    shim_path="$(find "$out/build" -path "*quickjs*/out/$shim" 2>/dev/null | head -n1)"
    if [ -z "$shim_path" ]; then
        shim_path="$(find "$out/build" -name "$shim" 2>/dev/null | head -n1)"
    fi
    if [ -z "$shim_path" ]; then
        echo "link-test: $shim not found under $out/build" >&2
        exit 1
    fi

    if [ "$is_windows" = "1" ]; then
        "$CC" -I "$header" "$src" "$out/$dll_import" -o "$work/hello_shared.exe"
        (cd "$out" && "$work/hello_shared.exe") > "$work/shared.out"
        assert_output "$work/shared.out" "$feature shared"

        "$CC" -I "$header" "$src" "$out/$staticlib" "$shim_path" \
            "${system_libs[@]}" -o "$work/hello_static.exe"
        "$work/hello_static.exe" > "$work/static.out"
        assert_output "$work/static.out" "$feature static"
    else
        "$CC" -I "$header" "$src" -L"$out" -lquickjs \
            -Wl,-rpath,"$PWD/$out" -o "$work/hello_shared"
        "$work/hello_shared" > "$work/shared.out"
        assert_output "$work/shared.out" "$feature shared"

        "$CC" -I "$header" "$src" "$out/$staticlib" "$shim_path" \
            "${system_libs[@]}" -o "$work/hello_static"
        "$work/hello_static" > "$work/static.out"
        assert_output "$work/static.out" "$feature static"
    fi

    echo "link-test: $feature shared+static OK"
done
