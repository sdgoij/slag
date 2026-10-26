#!/usr/bin/env bash
# Build a PGO-optimized `slag`, or regenerate the profile it uses.
#
# Two builds and one training pass. An instrumented build runs a training set and
# writes `.profraw` counters, `llvm-profdata merge` folds them into one profile,
# and the real build recompiles with `-Cprofile-use`. The source is unchanged;
# only branch weights and code layout move, which is where the win is: measured
# on the 77-row corpus, -19.7% with the corpus as the training set and -11.6% on
# 36 rows held out of training (see the perf journal).
#
# usage: tools/pgo.sh [--train] [DIR|FILE...]
#
#   --train     regenerate target/pgo.profdata from the given workloads (default
#               tools/corpus/workloads) before building. Without it the existing
#               profile is reused, and a missing profile is an error.
#
# Output: target/pgo/release/slag[.exe]. `target/release` is not touched, because
# PGO changes layout and cannot be mixed into an A/B of another change.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

train=0
if [ "${1-}" = "--train" ]; then
	train=1
	shift
fi
dirs=("$@")
if [ ${#dirs[@]} -eq 0 ]; then
	dirs=(tools/corpus/workloads)
fi

case "$(uname -s)" in
	MINGW* | MSYS* | CYGWIN*) exe=.exe ;;
	*) exe= ;;
esac

sysroot=$(rustc --print sysroot)
host=$(rustc -vV | sed -n 's/^host: //p')
profdata=$sysroot/lib/rustlib/$host/bin/llvm-profdata$exe
[ -x "$profdata" ] || {
	echo "pgo: no llvm-profdata at $profdata" >&2
	exit 1
}

raw=target/pgo-raw
profile=target/pgo.profdata

if [ "$train" = 1 ]; then
	echo "pgo: instrumented build"
	rm -rf "$raw"
	mkdir -p "$raw"
	CARGO_TARGET_DIR=target/pgo-instr \
		RUSTFLAGS="-Cprofile-generate=$root/$raw" \
		cargo build --quiet --release -p cli

	bin=target/pgo-instr/release/slag$exe
	[ -x "$bin" ] || { echo "pgo: no $bin" >&2; exit 1; }

	echo "pgo: training on ${dirs[*]}"
	for path in "${dirs[@]}"; do
		if [ -d "$path" ]; then
			# One process per directory: `--corpus` already walks it, and the
			# counters accumulate across a process's runs of one set.
			"$bin" --corpus "$path" >/dev/null
		else
			"$bin" "$path" >/dev/null
		fi
	done
	count=$(find "$raw" -name '*.profraw' | wc -l)
	[ "$count" -gt 0 ] || { echo "pgo: training produced no .profraw files" >&2; exit 1; }

	echo "pgo: merging $count raw profiles"
	# shellcheck disable=SC2046 # one path per file, none contain spaces
	"$profdata" merge -o "$profile" $(find "$raw" -name '*.profraw')
fi

[ -f "$profile" ] || { echo "pgo: no $profile (run with --train)" >&2; exit 1; }

echo "pgo: optimized build"
CARGO_TARGET_DIR=target/pgo \
	RUSTFLAGS="-Cprofile-use=$root/$profile" \
	cargo build --quiet --release -p cli

echo "pgo: target/pgo/release/slag$exe"
