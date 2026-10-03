#!/usr/bin/env bash
# Slag vs SpiderMonkey (js shell) on a workload directory.
#
# Usage: tools/corpus/sm-ab.sh [options]
#   --root DIR   a directory of *.js workloads — a family dir, e.g.
#                tools/corpus/workloads/opcost      (default: opcost)
#   --slag BIN   slag binary                       (default target/release/slag.exe)
#   --js BIN     SpiderMonkey js shell             (default scratch/jsshell/js.exe)
#   --out DIR    output directory                  (default scratch/sm-out)
#
# One shared process per engine (like corpus-ab.sh), so a late row can inherit
# heap state from an earlier one — pass a single family (the default) or a
# pre-isolated directory. Keys are the workload paths relative to --root; both
# engines are given that same root, so the keys line up.
#
# The js shell is a prebuilt nightly (it needs its DLLs/dylibs beside it):
#   curl -L -o jsshell.zip https://ftp.mozilla.org/pub/firefox/nightly/latest-mozilla-central/jsshell-win64.zip
#   unzip jsshell.zip -d scratch/jsshell       # jsshell-linux-x86_64.zip / jsshell-mac.zip elsewhere
#
# Prints the two engines' versions, then a per-workload gap table
# (gap = slag-jit / SM-jit; lower is better, <1 means slag faster) sorted by
# gap, and the result-parity verdict (all three columns returning the same
# bench() value).
#
# Exit: 0 clean, 2 setup error.
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=tools/corpus/workloads/opcost
out=scratch/sm-out
slag=target/release/slag.exe
js=scratch/jsshell/js.exe

while [ $# -gt 0 ]; do
  case "$1" in
    --root) root=$2; shift 2 ;;
    --slag) slag=$2; shift 2 ;;
    --js) js=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "sm-ab.sh: unknown option $1" >&2; exit 2 ;;
  esac
done

[ -x "$slag" ] || { echo "sm-ab.sh: no slag at $slag (cargo build --release -p cli)" >&2; exit 2; }
[ -x "$js" ] || { echo "sm-ab.sh: no js shell at $js" >&2; exit 2; }
[ -d "$root" ] || { echo "sm-ab.sh: no workload dir at $root" >&2; exit 2; }

mkdir -p "$out"
mapfile -t files < <(find "$root" -maxdepth 1 -name '*.js' -print | sort)
[ "${#files[@]}" -gt 0 ] || { echo "sm-ab.sh: no .js under $root" >&2; exit 2; }

"$slag" --corpus "$root" > "$out/slag-jit.tsv" 2>/dev/null
"$slag" --jitless --corpus "$root" > "$out/slag-jitless.tsv" 2>/dev/null
"$js" "$here/run_jsshell.js" jit "$root" "${files[@]}" > "$out/sm-jit.tsv" 2> "$out/sm-jit.err"

echo "slag: $slag  ($("$slag" --version 2>&1 | head -1))"
echo "js:   $js  ($("$js" --version 2>&1 | head -1))"
echo "root: $root  (${#files[@]} workloads)"
[ -s "$out/sm-jit.err" ] && echo "note: js stderr captured in $out/sm-jit.err"
echo
echo "gap = slag-jit / SM-jit (lower is better; <1 = slag faster)"
printf '%-9s %10s %12s %10s %-9s %s\n' gap slag_jit slag_jitless sm_jit parity workload
awk -F'\t' '
  FNR == 1 { file++ }
  file == 1 { if ($1 == "bench") { sj[$3] = $4; vj[$3] = $5 } next }
  file == 2 { if ($1 == "bench") { sl[$3] = $4; vl[$3] = $5 } next }
  file == 3 { if ($1 == "bench") { sm[$3] = $4; vm[$3] = $5 } next }
  END {
    for (k in sj) {
      if (k in sm && sm[k] > 0) { g = sj[k] / sm[k] } else { g = -1 }
      if (!(k in sm)) { p = "MISSING" }
      else if (vj[k] != vm[k] || (k in vl && vl[k] != vm[k])) { p = "MISMATCH" }
      else { p = "ok" }
      printf "%s\t%s\t%s\t%s\t%s\t%s\n", g, sj[k], (k in sl ? sl[k] : "-"), sm[k], p, k
    }
  }
' "$out/slag-jit.tsv" "$out/slag-jitless.tsv" "$out/sm-jit.tsv" \
  | sort -k1 -rn \
  | awk -F'\t' '{ printf "%-9.3f %10s %12s %10s %-9s %s\n", $1, $2, $3, $4, $5, $6 }'

exit 0
