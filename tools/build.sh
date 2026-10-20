#!/usr/bin/env bash
# Build `slag`, `sweep`, `wasmtest` (and `leak`), reusing a cache keyed by the
# content of the working tree. Keying on the commit would miss the common case
# here: bisects replace individual paths (`git checkout <rev> -- crates/`), so
# the compiled tree often matches no commit at all.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

known_packages=(cli test262 wasmtest)
selected=()

profile=release
cache=1
force=0
mode=build

fail() {
    echo "build: $*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
usage: tools/build.sh [options]

Builds release binaries into target/release, reusing a content-addressed cache
under target/build-cache (override with SLAG_BUILD_CACHE).

options:
  -p, --package <name>  build only this package (repeatable): cli, test262,
                        wasmtest. Default: all three.
                        cli -> slag, leak; test262 -> sweep; wasmtest -> wasmtest
  --debug               build the debug profile instead of release
  --release             build the release profile (default)
  --no-cache            always build; do not read or write the cache
  --force               ignore an existing cache entry and rebuild
  --print-key           print the cache key for the current tree, then exit
  -h, --help            show this help

The key is the hash of the working tree as `git write-tree` would record it,
plus the profile and toolchain. On a clean checkout it is `HEAD^{tree}`. A
dirty tree gets its own entry, so checking out single paths during a bisect
still hits the right binaries.

Packages are built one at a time because they resolve `runtime` with different
feature sets; a single invocation would unify those features and change the
binaries. Cache entries are shared across package selections: building only
`cli` leaves any `sweep` binary already stored for the same tree intact.
EOF
}

while [ $# -gt 0 ]; do
    arg=$1
    shift
    case "$arg" in
        -p|--package)
            [ $# -gt 0 ] || { echo "build: $arg needs a package name" >&2; exit 2; }
            selected+=("$1")
            shift
            ;;
        --package=*) selected+=("${arg#--package=}") ;;
        --debug) profile=debug ;;
        --release) profile=release ;;
        --no-cache) cache=0 ;;
        --force) force=1 ;;
        --print-key) mode=key ;;
        -h|--help) usage; exit 0 ;;
        *) echo "build: unknown argument: $arg" >&2; exit 2 ;;
    esac
done

if [ ${#selected[@]} -eq 0 ]; then
    selected=("${known_packages[@]}")
fi

wanted=()
for pkg in "${selected[@]}"; do
    case "$pkg" in
        cli) wanted+=(slag leak) ;;
        test262) wanted+=(sweep) ;;
        wasmtest) wanted+=(wasmtest) ;;
        *) fail "unknown package: $pkg (expected: ${known_packages[*]})" ;;
    esac
done

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) exe=.exe ;;
    *) exe= ;;
esac

# `git add` on a throwaway index snapshots the working tree without disturbing
# the real index. Untracked files that are not ignored are part of the key;
# the submodule is recorded as a gitlink, so its own HEAD is added explicitly
# (the build reads its fixtures) along with a hash of its status.
compute_key() {
    local index="$root/target/.build-cache-index"
    mkdir -p "$root/target" || fail "cannot create target/"

    local err tree
    # Seed from HEAD so recorded modes survive a host that does not track the
    # executable bit (core.filemode=false on Windows): `git add -A` on an empty
    # index would record every file as 644 and a clean tree would never hash to
    # HEAD^{tree}.
    if git rev-parse --verify HEAD >/dev/null 2>&1; then
        if ! err=$(GIT_INDEX_FILE="$index" git read-tree HEAD 2>&1); then
            printf 'build: cannot seed the cache index from HEAD:\n%s\n' "$err" >&2
            exit 1
        fi
    else
        rm -f "$index"
    fi
    if ! err=$(GIT_INDEX_FILE="$index" git add -A 2>&1 >/dev/null); then
        printf 'build: cannot snapshot the working tree:\n%s\n' "$err" >&2
        exit 1
    fi
    tree=$(GIT_INDEX_FILE="$index" git write-tree) || fail "git write-tree failed"
    [ -n "$tree" ] || fail "git write-tree produced an empty hash"

    local rustc_info flags sub cfg
    rustc_info=$(rustc -vV) || fail "rustc not found; install a Rust toolchain"
    flags=${RUSTFLAGS:-}${CARGO_ENCODED_RUSTFLAGS:-}
    sub=absent
    if [ -e "$root/test262/.git" ]; then
        sub=$(git -C "$root/test262" rev-parse HEAD 2>/dev/null || echo unknown)
        sub="$sub $(git -C "$root/test262" --no-optional-locks status --porcelain 2>/dev/null | git hash-object --stdin)"
    fi
    cfg=$(printf '%s\n' \
        "$rustc_info" \
        "profile=$profile" \
        "packages=${known_packages[*]}" \
        "root=$root" \
        "rustflags=$flags" \
        "rustc=${RUSTC:-}" \
        "target=${CARGO_BUILD_TARGET:-}" \
        "test262=$sub" |
        git hash-object --stdin) || fail "cannot hash the build configuration"

    printf '%s %s\n' "$tree" "$cfg"
}

if [ "$cache" = 1 ] || [ "$mode" = key ]; then
    read -r tree cfg <<<"$(compute_key)"
else
    tree=""
    cfg=""
fi

head=$(git rev-parse HEAD 2>/dev/null || echo no-head)
head_short=$(git rev-parse --short=12 HEAD 2>/dev/null || echo no-head)
if [ "$tree" = "$(git rev-parse HEAD^{tree} 2>/dev/null || echo none)" ]; then
    state=clean
else
    state=dirty
fi

if [ "$mode" = key ]; then
    cache_root=${SLAG_BUILD_CACHE:-$root/target/build-cache}
    entry="$cache_root/${tree:0:16}-${cfg:0:8}"
    printf 'tree %s\nconfig %s\nentry %s\nhead %s (%s)\n' \
        "$tree" "$cfg" "$entry" "$head_short" "$state"
    exit 0
fi

cache_root=${SLAG_BUILD_CACHE:-$root/target/build-cache}
entry="$cache_root/${tree:0:16}-${cfg:0:8}"

describe() {
    printf 'build-cache: %s %s/ %s, HEAD %s %s\n' \
        "$1" "${tree:0:16}" "$profile" "$head_short" "$state"
}

incomplete() {
    local dir=$1 bin
    [ -f "$dir/meta.txt" ] || return 0
    for bin in "${wanted[@]}"; do
        [ -f "$dir/$bin$exe" ] || return 0
    done
    return 1
}

restore() {
    local dir=$1 bin
    mkdir -p "$root/target/$profile"
    for bin in "${wanted[@]}"; do
        cp -f "$dir/$bin$exe" "$root/target/$profile/$bin$exe" ||
            fail "cannot restore target/$profile/$bin$exe (is it running?)"
    done
}

if [ "$cache" = 1 ] && [ "$force" = 0 ] &&
    [ -d "$entry" ] && ! incomplete "$entry"; then
    describe hit
    restore "$entry"
    exit 0
fi

if [ "$cache" = 1 ]; then
    describe miss
else
    printf 'build-cache: bypassed (%s, HEAD %s %s)\n' "$profile" "$head_short" "$state"
fi

build_args=(build --locked)
if [ "$profile" = release ]; then
    build_args+=(--release)
fi
for pkg in "${selected[@]}"; do
    cargo "${build_args[@]}" -p "$pkg"
done

for bin in "${wanted[@]}"; do
    [ -f "$root/target/$profile/$bin$exe" ] ||
        fail "cargo did not produce target/$profile/$bin$exe"
done

[ "$cache" = 1 ] || exit 0

after=$(compute_key)
if [ "$after" != "$tree $cfg" ]; then
    read -r tree cfg <<<"$after"
    entry="$cache_root/${tree:0:16}-${cfg:0:8}"
    printf 'build-cache: the tree changed during the build; storing under %s\n' \
        "${tree:0:16}"
fi

tmp="$entry.tmp.$$"
rm -rf "$tmp"
mkdir -p "$tmp"
if [ -d "$entry" ]; then
    for file in "$entry"/*; do
        [ -f "$file" ] || continue
        if [ "$(basename "$file")" = meta.txt ]; then
            continue
        fi
        cp -f "$file" "$tmp/"
    done
fi
for bin in "${wanted[@]}"; do
    cp -f "$root/target/$profile/$bin$exe" "$tmp/" ||
        fail "cannot copy target/$profile/$bin$exe into the cache"
done
stored=$(cd "$tmp" && ls | grep -v '^meta.txt$' | sed 's/\.exe$//' | tr '\n' ' ')
cat >"$tmp/meta.txt" <<EOF
tree $tree
config $cfg
head $head
head-short $head_short
state $state
profile $profile
rustc $(rustc -V)
built $(date -u +%Y-%m-%dT%H:%M:%SZ)
binaries $stored
EOF
rm -rf "$entry"
mv "$tmp" "$entry"
printf 'build-cache: stored %s\n' "$entry"
