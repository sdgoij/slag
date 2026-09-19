#!/usr/bin/env bash
# Per-family corpus run plus the JIT-pessimization gate.
#
# One process per (family, engine, mode). A single shared process lets late
# rows inherit an unbounded heap from earlier ones, and that is measurable:
# on 2026-09-19 the `language` family's try row read 816.7ms in-family against
# 267ms isolated in slag, and 74.3ms in-family against 37.4ms isolated in d8.
# Families named in --isolate instead run one workload per process, so the
# contamination cannot reach the reported number.
#
# The CLI's --corpus takes a directory (fs::read_dir), so isolation runs each
# row from a scratch directory holding only that file. The file keeps its
# sub-path inside the scratch root, which keeps the reported key identical to
# the non-isolated run — the keys are family-relative and the join depends on
# that.
#
# Writes one TSV per mode under --out plus a manifest recording which binaries
# and revision produced them, then runs jit-loss.awk over the two slag TSVs so
# a landed JIT regression fails this command.
#
# Usage: tools/corpus/corpus-ab.sh [options]
#   --out DIR        output directory           (default scratch/corpus-out)
#   --slag BIN       slag binary                (default target/release/slag.exe)
#   --d8 BIN         d8 binary                  (default v8/out/x64.release/d8.exe)
#   --root DIR       workload root              (default tools/corpus/workloads)
#   --isolate LIST   comma-separated families run one row per process
#                                                (default language)
#   --no-node        skip the node columns
#   --no-d8          skip the d8 columns
#   --no-table       skip the six-column gap table
#   --no-gate        skip the jit-loss gate
#   --recheck N      for each flagged row, re-run both modes isolated N times
#                    and compare the per-mode MINIMA; a row that clears on
#                    recheck was an environmental outlier, not a regression
#                                                (default 3; 0 disables)
#   --reuse          re-judge the TSVs already under --out without measuring
#
# Exit: 0 clean, 1 the gate confirmed a regression, 2 setup error.

set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)

root=tools/corpus/workloads
out=scratch/corpus-out
slag=target/release/slag.exe
d8=v8/out/x64.release/d8.exe
isolate=language
run_node_cols=1
run_d8_cols=1
run_gate=1
run_table=1
recheck=3
reuse=0
min_ms=5
margin=1.05

while [ $# -gt 0 ]; do
  case "$1" in
    --out) out=$2; shift 2 ;;
    --slag) slag=$2; shift 2 ;;
    --d8) d8=$2; shift 2 ;;
    --root) root=$2; shift 2 ;;
    --isolate) isolate=$2; shift 2 ;;
    --no-node) run_node_cols=0; shift ;;
    --no-d8) run_d8_cols=0; shift ;;
    --no-gate) run_gate=0; shift ;;
    --no-table) run_table=0; shift ;;
    --recheck) recheck=$2; shift 2 ;;
    --reuse) reuse=1; shift ;;
    --min-ms) min_ms=$2; shift 2 ;;
    --margin) margin=$2; shift 2 ;;
    -h|--help) sed -n '2,38p' "$0"; exit 0 ;;
    *) echo "corpus-ab.sh: unknown option $1" >&2; exit 2 ;;
  esac
done

[ -d "$root" ] || { echo "corpus-ab.sh: no workload root at $root" >&2; exit 2; }
[ -x "$slag" ] || { echo "corpus-ab.sh: no slag binary at $slag (cargo build --release -p cli)" >&2; exit 2; }
if [ "$run_d8_cols" = 1 ] && [ ! -x "$d8" ]; then
  echo "corpus-ab.sh: no d8 at $d8 — skipping the d8 columns" >&2
  run_d8_cols=0
fi
if [ "$run_node_cols" = 1 ] && ! command -v node >/dev/null 2>&1; then
  echo "corpus-ab.sh: node not on PATH — skipping the node columns" >&2
  run_node_cols=0
fi

mkdir -p "$out"
if [ "$reuse" = 0 ]; then
  : > "$out/slag-jit.tsv"
  : > "$out/slag-jitless.tsv"
  if [ "$run_node_cols" = 1 ]; then : > "$out/node-jit.tsv"; : > "$out/node-jitless.tsv"; fi
  if [ "$run_d8_cols" = 1 ]; then : > "$out/d8-jit.tsv"; : > "$out/d8-jitless.tsv"; fi

  # Record what produced the numbers. TSVs that outlive the binary they came
  # from are the freshness trap in its corpus form (a 12:15 run against an 11:39
  # binary is how a stored baseline goes stale without looking stale).
  mtime_of() { date -r "$1" +%Y-%m-%dT%H:%M:%S 2>/dev/null || echo unknown; }
  {
    echo "date: $(date +%Y-%m-%dT%H:%M:%S)"
    echo "head: $(git rev-parse HEAD 2>/dev/null || echo unknown)"
    echo "slag: $slag  mtime=$(mtime_of "$slag")"
    echo "d8: $d8  mtime=$(mtime_of "$d8")"
    echo "root: $root"
    echo "isolate: $isolate"
  } > "$out/manifest.txt"
else
  for need in slag-jit slag-jitless; do
    [ -f "$out/$need.tsv" ] || { echo "corpus-ab.sh: --reuse needs $out/$need.tsv" >&2; exit 2; }
  done
  echo "reusing existing TSVs in $out (measurement skipped)"
fi

is_isolated() {
  case ",$isolate," in *",$1,"*) return 0 ;; *) return 1 ;; esac
}

files_of() {  # family -> family-relative .js paths, sorted
  find "$root/$1" -name '*.js' -print | sed "s|^$root/$1/||" | sort
}

# The directory-based engines (slag, node) take the corpus dir; d8 takes an
# explicit file list because it has no directory-listing API.
run_dirs() {  # dir, label
  local dir=$1 label=$2
  "$slag" --corpus "$dir" >> "$out/slag-jit.tsv" || echo "corpus-ab.sh: slag jit failed for $label" >&2
  "$slag" --jitless --corpus "$dir" >> "$out/slag-jitless.tsv" || echo "corpus-ab.sh: slag jitless failed for $label" >&2
  if [ "$run_node_cols" = 1 ]; then
    node "$here/run_node.js" "$dir" >> "$out/node-jit.tsv" || echo "corpus-ab.sh: node jit failed for $label" >&2
    node --jitless "$here/run_node.js" "$dir" >> "$out/node-jitless.tsv" || echo "corpus-ab.sh: node jitless failed for $label" >&2
  fi
}

run_d8() {  # d8-root, files...
  [ "$run_d8_cols" = 1 ] || return 0
  local d8root=$1; shift
  [ $# -gt 0 ] || return 0
  "$d8" "$here/run_d8.js" -- jit "$d8root" "$@" >> "$out/d8-jit.tsv" || echo "corpus-ab.sh: d8 jit failed for $d8root" >&2
  "$d8" --jitless "$here/run_d8.js" -- jitless "$d8root" "$@" >> "$out/d8-jitless.tsv" || echo "corpus-ab.sh: d8 jitless failed for $d8root" >&2
}

if [ "$reuse" = 0 ]; then
  families=$(cd "$root" && ls -d */ 2>/dev/null | tr -d '/' | sort)
  [ -n "$families" ] || { echo "corpus-ab.sh: no families under $root" >&2; exit 2; }

  for family in $families; do
    if is_isolated "$family"; then
      echo "== $family (isolated: one process per row) =="
      iso="$out/iso/$family"
      while IFS= read -r rel; do
        rm -rf "$iso"
        mkdir -p "$iso/$(dirname "$rel")"
        cp "$root/$family/$rel" "$iso/$rel"
        run_dirs "$iso" "$family"
        run_d8 "$iso" "$iso/$rel"
      done < <(files_of "$family")
    else
      echo "== $family =="
      mapfile -t files < <(find "$root/$family" -name '*.js' -print | sort)
      run_dirs "$root/$family" "$family"
      run_d8 "$root/$family" "${files[@]}"
    fi
  done

  echo "corpus run complete: $out"
fi

# The table needs all six mode TSVs; with a column set skipped it cannot be
# built, so it is refused rather than printed half-empty.
if [ "$run_table" = 1 ]; then
  if [ "$run_node_cols" = 1 ] && [ "$run_d8_cols" = 1 ] \
    && [ -f "$out/d8-jit.tsv" ] && [ -f "$out/d8-jitless.tsv" ] \
    && [ -f "$out/node-jit.tsv" ] && [ -f "$out/node-jitless.tsv" ]; then
    echo "== gap table (sorted by slag/d8 jit; tiergain is a tiering signal, not a fold) =="
    printf '%-8s %-9s %-9s %-9s %-9s %-9s %-9s %-9s %s\n' \
      gap sl_jit sl_jl d8_jit d8_jl nd_jit nd_jl tiergain workload
    awk -f "$here/gap.awk" \
      "$out/slag-jit.tsv" "$out/slag-jitless.tsv" \
      "$out/d8-jit.tsv" "$out/d8-jitless.tsv" \
      "$out/node-jit.tsv" "$out/node-jitless.tsv" | sort -rn
  else
    echo "note: gap table skipped (needs both the d8 and node columns)" >&2
  fi
fi

if [ "$run_gate" = 1 ]; then
  echo "== jit-loss gate =="
  gate_out=$(awk -v MIN_MS="$min_ms" -v MARGIN="$margin" \
    -f "$here/jit-loss.awk" "$out/slag-jitless.tsv" "$out/slag-jit.tsv")
  gate_status=$?
  printf '%s\n' "$gate_out" | sort -r
  if [ "$gate_status" -eq 2 ]; then
    echo "jit-loss gate: input error" >&2
    exit 2
  fi
  if [ "$gate_status" -eq 0 ]; then
    exit 0
  fi

  # A single run cannot separate a real pessimization from an environmental
  # outlier: try/completion-values read 0.996x across five isolated runs of
  # this binary and 1.15x inside one full driver run. So a flagged row is
  # re-measured `recheck` times in each mode, isolated, and judged on the
  # per-mode MINIMA (the same min-of-N discipline the runners use for their
  # timed samples). A row that clears on recheck was noise.
  if [ "$recheck" -eq 0 ]; then
    echo "jit-loss gate: recheck disabled; treating every flag as a regression" >&2
    exit 1
  fi

  confirmed=0
  while IFS= read -r line; do
    case "$line" in
      "JIT LOSS"*) ;;
      *) continue ;;
    esac
    key=$(printf '%s' "$line" | awk -F'  ' '{print $NF}')
    src=$(find "$root" -path "*/$key" -print 2>/dev/null | head -n 1)
    if [ -z "$src" ]; then
      echo "recheck: cannot locate $key under $root; flag kept" >&2
      confirmed=$((confirmed + 1))
      continue
    fi
    fam=${src#"$root"/}; fam=${fam%%/*}
    rs="$out/recheck/$fam"
    rm -rf "$rs"; mkdir -p "$rs/$(dirname "$key")"; cp "$src" "$rs/$key"
    minj=""; minl=""; n=1
    while [ "$n" -le "$recheck" ]; do
      j=$(  "$slag" --corpus "$rs"           | awk -F'\t' '/^bench/{print $4; exit}' )
      l=$(  "$slag" --jitless --corpus "$rs" | awk -F'\t' '/^bench/{print $4; exit}' )
      if [ -n "$j" ] && { [ -z "$minj" ] || awk -v a="$j" -v b="$minj" 'BEGIN{exit !(a < b)}'; }; then minj=$j; fi
      if [ -n "$l" ] && { [ -z "$minl" ] || awk -v a="$l" -v b="$minl" 'BEGIN{exit !(a < b)}'; }; then minl=$l; fi
      n=$((n + 1))
    done
    if [ -z "$minj" ] || [ -z "$minl" ]; then
      echo "recheck: $key produced no measurement; flag kept" >&2
      confirmed=$((confirmed + 1))
      continue
    fi
    r=$(awk -v a="$minj" -v b="$minl" 'BEGIN{printf "%.2f", a / b}')
    if awk -v r="$r" -v m="$margin" 'BEGIN{exit !(r > m)}'; then
      printf 'CONFIRMED JIT LOSS  %sx  jit_min=%sms  jitless_min=%sms  %s\n' \
        "$r" "$minj" "$minl" "$key"
      confirmed=$((confirmed + 1))
    else
      printf 'cleared on recheck  %sx (best of %s runs each mode)  %s\n' \
        "$r" "$recheck" "$key"
    fi
  done <<EOF
$gate_out
EOF

  if [ "$confirmed" -gt 0 ]; then
    printf 'jit-loss gate: %d row(s) confirmed after recheck\n' "$confirmed"
    exit 1
  fi
  echo "jit-loss gate: clean after recheck"
fi
exit 0
