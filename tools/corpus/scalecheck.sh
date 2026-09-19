#!/usr/bin/env bash
# Ground-truth fold test: run each workload at N and at 4N and check whether the
# engine's time scales with the work.
#
#   4x scaling  -> the work is real
#   flat        -> the engine eliminated it
#
# --jitless runs as a control: an interpreter cannot eliminate the work, so it
# must scale ~4x on every row. A jitless column that does not scale means this
# test is broken, not that the engine folded something.
#
# Why this exists. The machine-code and per-iteration signals are both
# ambiguous. A loop that is unrolled or vectorized runs at about a cycle per
# iteration while still containing a back edge, so neither "no back edge"
# (foldcheck.awk) nor "sub-nanosecond per iteration" proves elimination. Both
# are consistent with a loop V8 simply compiled well. Quadrupling the work is
# the only signal that settles it, and it is what produced the 2026-09-19
# result below.
#
# Measured 2026-09-19 (V8 15.6.0, this machine): every row scales ~3.7-4.1x,
# so V8 eliminates NONE of these workloads — including `destructure`,
# `try_catch_loop` and `push_pop`, which earlier notes described as
# closed-form-folded. The large slag/d8 gaps on those rows are real work, not
# an artifact of the comparison.
#
# The table is per-row and explicit: a workload's iteration count is a literal
# in its own source, so there is nothing generic to rewrite. Add a row by
# giving <root-relative path>|<sed expression>.
#
# Usage: tools/corpus/scalecheck.sh [--d8 BIN] [--out DIR]
#
# Exit: 0 every row scaled (JIT and control), 2 setup error, 1 otherwise.

set -uo pipefail

d8=v8/out/x64.release/d8.exe
out=scratch/scale-out
rows=(
  "control/try_catch_loop.js|s/i < 1500000/i < 6000000/"
  "objects/destructure.js|s/i < 1000000/i < 4000000/"
  "arrays/typed_array.js|s/k < 2000000/k < 8000000/"
  "builtins/object_keys.js|s/i < 20000/i < 80000/"
  "calls/recursive_fib.js|s/i < 1200/i < 4800/"
  "language/statements/for-in/head-let-fresh-binding-per-iteration.js|s/__t262Iter < 100000/__t262Iter < 400000/"
  "builtins/map_churn.js|s/i < 300000/i < 1200000/"
  "control/switch_dispatch.js|s/i < 3000000/i < 12000000/"
)

while [ $# -gt 0 ]; do
  case "$1" in
    --d8) d8=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    -h|--help) sed -n '2,38p' "$0"; exit 0 ;;
    *) echo "scalecheck.sh: unknown option $1" >&2; exit 2 ;;
  esac
done

here=$(cd "$(dirname "$0")" && pwd)
root=tools/corpus/workloads
[ -x "$d8" ] || { echo "scalecheck.sh: no d8 at $d8" >&2; exit 2; }

rm -rf "$out"
mkdir -p "$out/base" "$out/quad"

for r in "${rows[@]}"; do
  rel=${r%%|*}
  expr=${r#*|}
  src="$root/$rel"
  if [ ! -f "$src" ]; then
    echo "scalecheck.sh: no workload at $src (table entry skipped)" >&2
    continue
  fi
  flat=$(printf '%s' "$rel" | tr '/' '_')
  cp "$src" "$out/base/$flat"
  sed "$expr" "$src" > "$out/quad/$flat"
  # A rewrite that did not apply leaves the two copies identical, which would
  # read as a FOLDED verdict rather than as the table error it is.
  if cmp -s "$src" "$out/quad/$flat"; then
    echo "scalecheck.sh: the rewrite for $rel did not apply; fix its table entry" >&2
    rm -f "$out/quad/$flat"
    continue
  fi
done

"$d8"           "$here/run_d8.js" -- jit     "$out" "$out"/base/*.js "$out"/quad/*.js > "$out/jit.txt" 2>&1
"$d8" --jitless "$here/run_d8.js" -- jitless "$out" "$out"/base/*.js "$out"/quad/*.js > "$out/jl.txt" 2>&1

declare -A jb=() jq=() lb=() lq=()

read_into() {  # dump-file, array prefix
  local file=$1 prefix=$2 tag mode path ms res name
  while IFS=$'\t' read -r tag mode path ms res; do
    [ "$tag" = bench ] || continue
    case "$path" in
      base/*) name=${path#base/}; printf -v "${prefix}b[$name]" '%s' "$ms" ;;
      quad/*) name=${path#quad/}; printf -v "${prefix}q[$name]" '%s' "$ms" ;;
    esac
  done < "$file"
}

read_into "$out/jit.txt" j
read_into "$out/jl.txt" l

ratio() { awk -v base="$1" -v quad="$2" 'BEGIN { if (base + 0 > 0) printf "%.2f", quad / base; else print "-" }'; }
# A row counts as scaling if it grew by at least half the factor; below that it
# is flat enough to suspect elimination, and the control column says whether
# the test itself was sound.
scaled() { awk -v r="$1" 'BEGIN { exit !(r + 0 >= 2.0) }'; }

printf '%-46s %-9s %-9s %-6s %-9s %-9s %-6s %s\n' row base_jit quad_jit x4_jit base_jl quad_jl x4_jl verdict
fail=0
for f in "$out"/base/*.js; do
  name=$(basename "$f")
  [ -n "${jb[$name]:-}" ] && [ -n "${lb[$name]:-}" ] || continue
  rj=$(ratio "${jb[$name]}" "${jq[$name]:-0}")
  rl=$(ratio "${lb[$name]}" "${lq[$name]:-0}")
  verdict=real
  if ! scaled "$rl"; then
    verdict=TEST-BROKEN
    fail=1
  elif ! scaled "$rj"; then
    verdict=FOLDED
    fail=1
  fi
  printf '%-46s %-9s %-9s %-6s %-9s %-9s %-6s %s\n' "$name" \
    "${jb[$name]}" "${jq[$name]:-?}" "$rj" \
    "${lb[$name]}" "${lq[$name]:-?}" "$rl" "$verdict"
done

if [ "$fail" -ne 0 ]; then
  echo "scalecheck: a row did not scale, or the jitless control did not scale" >&2
  exit 1
fi
exit 0
