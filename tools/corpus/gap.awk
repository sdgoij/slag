# Six-column corpus gap table.
#
# Joins the six mode TSVs written by corpus-ab.sh (lines:
# bench<TAB>mode<TAB>path<TAB>ms<TAB>result) and prints one row per workload,
# keyed by path. The first field is the slag/d8 gap, so `sort -rn` on the
# output is the V8-distance ranking.
#
# Columns (after the sort key):
#   sl_jit  slag compiled
#   sl_jl   slag --jitless            sl_jit/sl_jl > 1.05 is a JIT regression
#   d8_jit  d8 compiled
#   d8_jl   d8 --jitless
#   nd_jit  node compiled             cross-check on the d8 column
#   nd_jl   node --jitless
#   tiergain  d8_jitless/d8_jit  how much V8's OPTIMIZING TIER buys on this
#            row. This is a tiering signal, NOT a fold signal: a tight loop
#            whose interpreter path is slow and whose compiled path is fast
#            shows a large value while still doing every iteration of work.
#            Measured 2026-09-19 with tools/corpus/scalecheck.sh: every corpus
#            row (including `destructure`, `try_catch_loop` and `push_pop`)
#            scales ~4x in d8 when its work is quadrupled, i.e. V8 eliminates
#            none of them. Read `tiergain` as "how much headroom V8's compiler
#            has here", and use scalecheck.sh before claiming any row is free.
#
# Missing cells print as "-": a row absent from one mode's TSV means that run
# diverged or was truncated, and the row should not be trusted until re-run.
#
# Usage:
#   awk -f gap.awk <slag-jit> <slag-jitless> <d8-jit> <d8-jitless> \
#                  <node-jit> <node-jitless> | sort -rn
#
# Exit: 0 ok, 2 usage error (the caller checks the pipeline status).

BEGIN {
  FS = "\t"
  if (ARGC != 7) {
    print "usage: awk -f gap.awk <slag-jit> <slag-jitless> <d8-jit> <d8-jitless> <node-jit> <node-jitless>" > "/dev/stderr"
    # `exit` in BEGIN still runs END, so the failure is latched and re-raised.
    fatal = 1
  }
}

/^bench/ {
  if (NF < 4) next
  key = $3
  ms = $4 + 0
  # The jitless checks must come first: "slag-jitless" contains "slag-jit",
  # so the compiled check would otherwise swallow it.
  if (index(FILENAME, "slag-jitless") > 0) sl[key] = ms
  else if (index(FILENAME, "slag-jit") > 0) sj[key] = ms
  else if (index(FILENAME, "d8-jitless") > 0) dl[key] = ms
  else if (index(FILENAME, "d8-jit") > 0) dj[key] = ms
  else if (index(FILENAME, "node-jitless") > 0) nl[key] = ms
  else if (index(FILENAME, "node-jit") > 0) nj[key] = ms
  seen[key] = 1
}

function cell(v) { return (v == "" ? "-" : sprintf("%.2f", v)) }

END {
  if (fatal) exit 2
  if (length(seen) == 0) {
    print "gap.awk: no rows read — the input TSVs are empty or were not produced" > "/dev/stderr"
    exit 2
  }
  for (key in seen) {
    # Sort key first so `sort -rn` ranks by V8 distance; a row missing either
    # compiled time sorts last.
    gap = (sj[key] != "" && dj[key] != "" && dj[key] > 0) ? sj[key] / dj[key] : -1
    tier = (dj[key] != "" && dj[key] > 0 && dl[key] != "") ? dl[key] / dj[key] : -1
    printf "%.2f\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n",
      gap, cell(sj[key]), cell(sl[key]), cell(dj[key]), cell(dl[key]),
      cell(nj[key]), cell(nl[key]), (tier < 0 ? "-" : sprintf("%.2f", tier)), key
  }
}
