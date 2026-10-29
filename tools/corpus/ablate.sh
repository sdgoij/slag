#!/usr/bin/env bash
# Per-variant cost table: one workload shape ablated into variants, each run in
# its own process on all four engine/mode combinations.
#
#   tools/corpus/ablate.sh <dir> [slag-binary]
#
# Use it to put a *time* on each component of a hot row instead of guessing from
# IR or counts. Write the variants as self-contained workloads (`function
# bench() {...} bench();`, one component removed per file) and read the columns
# as deltas between variants.
#
# The `jl` columns are the load-bearing ones: both engines interpret the same
# work there, so V8's optimizer cannot flatter the number. A component whose jl
# ratio is high is runtime machinery (fixable in the interpreter); a component
# whose jit ratio is much higher than its jl ratio is one V8's optimizer removes.
#
# One process per variant on purpose: a whole-directory run shares an allocation
# context and inflates later rows (tools/corpus/README.md).

set -uo pipefail

dir=${1:?usage: ablate.sh <dir> [slag-binary]}
slag=${2:-target/release/slag.exe}
node=${NODE_BIN:-node}
runner=$(cd "$(dirname "$0")" && pwd)/run_node.js
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# `bench <mode> <file> <ms> <result>` on stdout.
ms() {
  local out
  out=$("$@" 2>/dev/null | tr -d '\r' | awk -F'\t' '$1=="bench"{print $4; exit}')
  printf '%s' "${out:-}"
}

printf '%-24s %10s %10s %10s %10s %8s %8s\n' variant slag_jit slag_jl node_jit node_jl jit_x jl_x
for f in "$dir"/*.js; do
  [ -e "$f" ] || continue
  one=$scratch/one
  rm -rf "$one" && mkdir -p "$one" && cp "$f" "$one/"
  sj=$(ms "$slag" --corpus "$one")
  sl=$(ms "$slag" --jitless --corpus "$one")
  nj=$(ms "$node" "$runner" "$one")
  nl=$(ms "$node" --jitless "$runner" "$one")
  jit_x=$(awk -v a="$sj" -v b="$nj" 'BEGIN{ if (b+0 > 0) printf "%.1f", a/b; else printf "-" }')
  jl_x=$(awk -v a="$sl" -v b="$nl" 'BEGIN{ if (b+0 > 0) printf "%.1f", a/b; else printf "-" }')
  printf '%-24s %10s %10s %10s %10s %8s %8s\n' "$(basename "$f")" "$sj" "$sl" "$nj" "$nl" "$jit_x" "$jl_x"
done
