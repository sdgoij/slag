#!/usr/bin/env bash
# Run every corpus workload in its own process and collect the engine's own
# counters: the JIT refusal census (`SLAG_JIT_REPORT`) and, with --helpers, the
# per-helper call histogram (`JIT_HELPER_STATS`).
#
# One process per file, because `--corpus DIR` runs every row in one process and
# the counters would be summed across rows.
#
# usage: tools/opcount.sh [--helpers] [DIR]
#
# Output is TSV on stdout, one line per (row, counter), so it can be sorted,
# grouped or diffed between two builds:
#   row <TAB> jit <TAB> refused <TAB> <tier> <TAB> <reason> <TAB> <bodies> <TAB> <steps>
#   row <TAB> jit <TAB> compiled <TAB> <bodies>
#   row <TAB> jit <TAB> deopts <TAB> <count>
#   row <TAB> helper <TAB> <count> <TAB> <name>          (with --helpers)
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

helpers=0
if [ "${1-}" = "--helpers" ]; then
	helpers=1
	shift
fi
dir=${1:-tools/corpus/workloads}
bin=${SLAG_BIN:-target/release/slag.exe}

[ -d "$dir" ] || { echo "opcount: no such directory: $dir" >&2; exit 2; }
[ -f "$bin" ] || { echo "opcount: no such binary: $bin" >&2; exit 2; }

# The switches are read by the engine at run time; the helper counter also has
# to be on when each body is *compiled*, which happens during this same run.
env_args=(SLAG_JIT_REPORT=1)
[ "$helpers" = 1 ] && env_args+=(JIT_HELPER_STATS=1)

find "$dir" -name '*.js' | sort | while read -r file; do
	row=${file#"$dir"/}
	out=$(env "${env_args[@]}" "$bin" "$file" 2>/dev/null || true)
	printf '%s\n' "$out" |
		awk -F'\t' -v row="$row" 'BEGIN { OFS = "\t" } $1 == "jit" || $1 == "helper" { print row, $0 }'
done
