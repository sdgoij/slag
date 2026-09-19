# JIT-pessimization gate.
#
# Joins the two slag mode TSVs (lines: bench<TAB>mode<TAB>path<TAB>ms<TAB>result)
# and flags every row where the compiled path is SLOWER than --jitless on the
# same binary. This is a regression check, not a missing-optimization check:
# both modes run the same source on the same engine, so a loss here is
# attributable to the JIT and not to the workload.
#
# Thresholds:
#   MIN_MS  rows below this are dominated by timer quantization and the
#           runner's sample floor; they are skipped rather than judged.
#   MARGIN  ratio above which a row is flagged. The 2026-09-19 run put the
#           noise floor at 2% (search_slice 1.02 and spread_assign 0.99 with
#           no known pessimization in the tree), so 1.05 leaves headroom.
#
# A row present in one mode's TSV and absent from the other is reported as
# INCOMPLETE (a diverged run, not a pessimization) and also fails the gate.
#
# Usage:
#   awk -v MIN_MS=5 -v MARGIN=1.05 -f jit-loss.awk <jitless.tsv> <jit.tsv>
#
# Exit: 0 clean, 1 flagged, 2 usage error.

BEGIN {
  FS = "\t"
  if (MIN_MS == "") MIN_MS = 5
  if (MARGIN == "" || MARGIN + 0 <= 0) MARGIN = 1.05
  if (ARGC != 3) {
    print "usage: awk -v MIN_MS=<ms> -v MARGIN=<ratio> -f jit-loss.awk <jitless.tsv> <jit.tsv>" > "/dev/stderr"
    # `exit` in BEGIN still runs END, so the failure is latched and re-raised
    # there; without this the END block would report the empty read as clean
    # and overwrite the exit status — a gate that silently passes.
    fatal = 1
  }
}

/^bench/ {
  if (NF < 4) next
  key = $3
  # The caller passes the jitless TSV first; the file name is the only
  # discriminator available, since both files carry the same mode column.
  if (index(FILENAME, "jitless") > 0) sl[key] = $4 + 0
  else sj[key] = $4 + 0
  seen[key] = 1
}

END {
  if (fatal) exit 2
  if (length(seen) == 0) {
    print "jit-loss gate: no rows read — the input TSVs are empty or were not produced" > "/dev/stderr"
    exit 2
  }
  bad = 0
  for (key in seen) {
    if (!(key in sl) || !(key in sj)) {
      printf "INCOMPLETE  jit=%s jitless=%s  %s\n",
        (key in sj ? sj[key] : "-"), (key in sl ? sl[key] : "-"), key
      bad++
      continue
    }
    if (sl[key] <= 0) continue
    ratio = sj[key] / sl[key]
    if (sj[key] > MIN_MS && ratio > MARGIN) {
      printf "JIT LOSS  %.2fx  jit=%.2fms  jitless=%.2fms  %s\n",
        ratio, sj[key], sl[key], key
      bad++
    }
  }
  if (bad == 0) {
    print "jit-loss gate: clean"
    exit 0
  }
  printf "jit-loss gate: %d row(s) flagged\n", bad
  exit 1
}
