#!/bin/sh
# Slag vs Brimstone: one process per (engine, workload), N rounds with the
# engine order rotated each round, minimum per cell.
#
#   tools/brimstone/compare.sh [rounds]
#   BS=/path/to/bs.exe tools/brimstone/compare.sh 5
#
# The workloads print "<name> <ms> ...", so the second field is the timed span
# (the process's own startup and parse are excluded). Two engines are Slag
# (`--jit` is the default; `--jitless` is the interpreter); the third is
# Brimstone's `bs`, a bytecode interpreter with no JIT, so `slag-jit` vs `bs`
# is "our engine" and `slag-njit` vs `bs` is "interpreter vs interpreter".
#
# The suite is deliberately mixed: a row whose loop sits at the TOP LEVEL of the
# script is not the same measurement as the same loop inside a function (see
# `.notes/perf.md`, "top-level script loops never reach the JIT"), so both
# shapes are present: `arith`/`array`/`forin`/`method`/`object` are top-level,
# `fn_*` are the same or similar shapes inside a function, and
# `script_only`/`script_fn`/`script_call` isolate one cause.
set -u
rounds=${1:-5}
BS=${BS:-/c/Users/T/src/github.com/sdgoij/brimstone/target/release/bs.exe}
SLAG=${SLAG:-target/release/slag.exe}
dir=$(dirname "$0")/suite
out=$(mktemp -d)

run_one() {
	engine=$1
	file=$2
	case $engine in
	bs) out_text=$("$BS" "$file" 2>/dev/null) ;;
	jit) out_text=$("$SLAG" "$file" 2>/dev/null) ;;
	interp) out_text=$("$SLAG" --jitless "$file" 2>/dev/null) ;;
	esac
	ms=$(echo "$out_text" | awk '{ print $2 }' | head -1)
	[ -n "$ms" ] || ms=0
	echo "$(basename "$file" .js)	$ms" >> "$out/$engine.tsv"
}

i=0
while [ "$i" -lt "$rounds" ]; do
	for file in "$dir"/*.js; do
		# Rotate the engine order so a fixed-position bias cancels out.
		case $((i % 3)) in
		0) run_one bs "$file"; run_one jit "$file"; run_one interp "$file" ;;
		1) run_one interp "$file"; run_one bs "$file"; run_one jit "$file" ;;
		*) run_one jit "$file"; run_one interp "$file"; run_one bs "$file" ;;
		esac
	done
	i=$((i + 1))
done

read_min() {
	awk -F'\t' -v key="$1" '
	  { if ($1 == key && (best == "" || $2 + 0 < best)) best = $2 + 0 }
	  END { print (best == "" ? 0 : best) }
	' "$2"
}

printf '%-12s %9s %9s %9s %8s %8s\n' bench bs slag-jit slag-njit jit/bs njit/bs
for file in "$dir"/*.js; do
	name=$(basename "$file" .js)
	b=$(read_min "$name" "$out/bs.tsv")
	j=$(read_min "$name" "$out/jit.tsv")
	n=$(read_min "$name" "$out/interp.tsv")
	printf '%-12s %9s %9s %9s %8s %8s\n' "$name" "$b" "$j" "$n" \
		"$(awk -v a="$j" -v b="$b" 'BEGIN { printf "%.2f", (b > 0 ? a / b : 0) }')" \
		"$(awk -v a="$n" -v b="$b" 'BEGIN { printf "%.2f", (b > 0 ? a / b : 0) }')"
done
rm -rf "$out"
