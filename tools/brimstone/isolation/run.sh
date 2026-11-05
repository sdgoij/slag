#!/bin/sh
# The script-certification isolation: the same 10M-iteration loop with no call
# (`script_only`), with a call to a certified top-level function
# (`script_fn`), and with `Date.now`/`console.log` (`script_call`). These print
# nothing (a print IS the disqualifying member call), so the harness times the
# whole process — startup is ~30 ms of the delta, which is why the interesting
# comparison is script_only vs script_call, not the absolute numbers.
#
#   sh tools/brimstone/isolation/run.sh
set -u
BS=${BS:-/c/Users/T/src/github.com/sdgoij/brimstone/target/release/bs.exe}
SLAG=${SLAG:-target/release/slag.exe}
dir=$(dirname "$0")

best() {
	cmd=$1
	file=$2
	best=9999999
	for _ in 1 2 3; do
		start=$(date +%s%N)
		$cmd "$file" >/dev/null 2>&1
		end=$(date +%s%N)
		t=$(((end - start) / 1000000))
		[ "$t" -lt "$best" ] && best=$t
	done
	echo "$best"
}

printf '%-14s %8s %10s %10s\n' script bs slag-jit slag-njit
for name in script_only script_fn script_call; do
	file=$dir/$name.js
	printf '%-14s %8s %10s %10s\n' "$name" \
		"$(best "$BS" "$file")" \
		"$(best "$SLAG" "$file")" \
		"$(best "$SLAG --jitless" "$file")"
done
