#!/usr/bin/env bash
# Per-row fold prover: decide whether V8 eliminated a workload's work, from
# the machine code TurboFan produced, rather than inferring it from timing.
#
# Why it exists: whether V8 has eliminated a workload's work cannot be read off
# a timing ratio. The gap table's `tiergain` column (d8_jitless/d8_jit) is a
# TIERING signal, not a fold signal - a tight loop whose interpreter path is
# slow and whose compiled path is fast shows a large value while still doing
# every iteration of its work. This tool reads the code instead: it forces the
# workload into TurboFan with --print-opt-code and checks whether a back edge
# survives. A folded loop compiles to straight-line code and has none.
#
# Measured 2026-09-19: all 41 corpus rows report LOOP, and
# tools/corpus/scalecheck.sh independently confirms every sampled row scales
# ~4x when its work is quadrupled. No corpus workload is folded by V8, so the
# large gaps in the table are real work. Treat a FOLDED verdict here as a
# strong claim and confirm it with scalecheck.sh before acting on it.
#
# One workload per d8 process, so the dump contains only that workload's
# functions and needs no section-to-row association.
#
# Usage: tools/corpus/foldcheck.sh [options] [file.js...]
#   --root DIR   workload root      (default tools/corpus/workloads)
#   --d8 BIN     d8 binary          (default v8/out/x64.release/d8.exe)
#   --tsv DIR    TSV dir to annotate each row with tiergain
#                                   (default scratch/corpus-out; "-" to skip)
#   --filter RE  only rows whose root-relative path matches RE
#
# With no file arguments, every *.js under --root is checked.
#
# Columns: verdict  bench  total  tiergain  workload
#   verdict   LOOP (a back edge: the work is there) | FOLDED (no back edge) |
#             NO-DUMP (bench never optimized: no conclusion) | ERROR
#   bench     back edges in `bench` itself
#   total     back edges anywhere in the dump (the verdict is driven by this,
#             so a loop in a helper the workload calls still counts)
#   tiergain  the gap table's d8_jitless/d8_jit for the same row, shown for
#             context only - it is a tiering signal and does not indicate a
#             fold either way
#
# Exit: 0 all rows produced a verdict, 1 some row could not be measured.

set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)

root=tools/corpus/workloads
d8=v8/out/x64.release/d8.exe
tsv=scratch/corpus-out
filter=
files=()

while [ $# -gt 0 ]; do
  case "$1" in
    --root) root=$2; shift 2 ;;
    --d8) d8=$2; shift 2 ;;
    --tsv) tsv=$2; shift 2 ;;
    --filter) filter=$2; shift 2 ;;
    -h|--help) sed -n '2,45p' "$0"; exit 0 ;;
    -*) echo "foldcheck.sh: unknown option $1" >&2; exit 2 ;;
    *) files+=("$1"); shift ;;
  esac
done

[ -x "$d8" ] || { echo "foldcheck.sh: no d8 at $d8" >&2; exit 2; }

if [ "${#files[@]}" -eq 0 ]; then
  while IFS= read -r f; do files+=("$f"); done < <(find "$root" -name '*.js' -print | sort)
fi

tier_gain() {  # family-relative key -> d8_jitless/d8_jit, or "-"
  local key=$1 dj dl
  [ -n "$tsv" ] && [ -f "$tsv/d8-jit.tsv" ] && [ -f "$tsv/d8-jitless.tsv" ] || return 0
  dj=$(awk -F'\t' -v k="$key" '/^bench/ && $3 == k { print $4; exit }' "$tsv/d8-jit.tsv")
  dl=$(awk -F'\t' -v k="$key" '/^bench/ && $3 == k { print $4; exit }' "$tsv/d8-jitless.tsv")
  if [ -n "$dj" ] && [ -n "$dl" ]; then
    awk -v a="$dl" -v b="$dj" 'BEGIN { if (b > 0) printf "%.2f", a / b; else print "-" }'
  else
    printf -- '-'
  fi
}

printf '%-9s %-6s %-6s %-9s %s\n' verdict bench total tiergain workload
loop=0; folded=0; nodump=0; errored=0

for f in "${files[@]}"; do
  rel=${f#"$root"/}
  if [ -n "$filter" ] && ! printf '%s' "$rel" | grep -q -- "$filter"; then
    continue
  fi
  # The TSVs are keyed relative to the family dir, not the workload root.
  vjl=$(tier_gain "${rel#*/}")

  dump=$("$d8" --allow-natives-syntax --print-opt-code "$here/foldcheck.js" -- "$f" 2>/dev/null)
  res=$(printf '%s\n' "$dump" | awk -f "$here/foldcheck.awk")

  if [ -z "$res" ]; then
    printf '%-9s %-6s %-6s %-9s %s\n' "ERROR" "-" "-" "$vjl" "$rel"
    errored=$((errored + 1))
    continue
  fi
  verdict=${res%%$'\t'*}
  rest=${res#*$'\t'}
  bedges=${rest%%$'\t'*}
  tedges=${rest#*$'\t'}
  case "$verdict" in
    LOOP) loop=$((loop + 1)) ;;
    FOLDED) folded=$((folded + 1)) ;;
    NO-DUMP) nodump=$((nodump + 1)) ;;
  esac
  printf '%-9s %-6s %-6s %-9s %s\n' "$verdict" "$bedges" "$tedges" "$vjl" "$rel"
done

printf 'summary: %d LOOP (work present), %d FOLDED (V8 eliminated it), %d NO-DUMP, %d ERROR\n' \
  "$loop" "$folded" "$nodump" "$errored"

if [ "$errored" -gt 0 ] || [ "$nodump" -gt 0 ]; then
  exit 1
fi
exit 0
