# Detects whether an optimized function's machine code still contains a loop,
# from d8's --print-opt-code dump (see foldcheck.js for how the dump is
# produced).
#
# d8 prints one block per optimized function:
#
#   --- Optimized code ---
#   name = bench
#   Instructions (size = 572)
#   0x7ff6e0040040     0  55                   push rbp
#   0x7ff6e0040053    13  0f8624010000         jna 00007FF6E004017D  <+0x13d>
#
# Each instruction line carries its byte offset (field 2, hex) and, for a
# branch, its target as <+0xOFFSET>. A branch whose target offset is BELOW its
# own offset is a back edge: a loop that survived optimization. A function V8
# closed-form-folded compiles to straight-line code and has none, which is the
# signature this file exists to detect.
#
# Verdicts:
#   LOOP     at least one back edge in the dump — the work is still there
#   FOLDED   bench was optimized and the dump has no back edge anywhere
#   NO-DUMP  bench never reached the optimizing tier (an unsuitable body, e.g.
#            one containing eval, or --allow-natives-syntax was not passed):
#            nothing can be concluded either way
#
# Back edges are counted across EVERY function in the dump, not just `bench`,
# because a workload's work may sit in a helper the runner calls rather than in
# bench itself. That biases the verdict toward LOOP, i.e. toward "this row is a
# real measurement", which is the safe direction: it can only under-report a
# fold, never invent one. The `bench` count is reported separately so a caller
# can see where the loop actually is.
#
# Prints one line: <verdict><TAB><bench back edges><TAB><total back edges>

function hex(s,   i, c, v, n) {
  n = 0
  for (i = 1; i <= length(s); i++) {
    c = tolower(substr(s, i, 1))
    v = index("0123456789abcdef", c) - 1
    if (v < 0) return -1
    n = n * 16 + v
  }
  return n
}

/^name = / { cur = $3; sect[cur] = 1; next }

/^Instructions \(size/ { next }

# Only instruction lines: an address, whitespace, then the offset column.
# Lines in the constant pool carry a trailing colon and do not match.
/^0x[0-9a-fA-F]+[ \t]/ {
  if (cur == "") next
  off = hex($2)
  if (off < 0) next
  if (match($0, /<\+0x[0-9a-fA-F]+>/)) {
    # The match is `<+0xOFFSET>`; skip the four characters `<+0x` and drop
    # the trailing `>`.
    tgt = hex(substr($0, RSTART + 4, RLENGTH - 5))
    if (tgt >= 0 && tgt < off) {
      back[cur]++
      total++
    }
  }
}

END {
  if (!("bench" in sect)) {
    print "NO-DUMP\t0\t" (total + 0)
    exit
  }
  if (total + 0 > 0) {
    print "LOOP\t" (back["bench"] + 0) "\t" (total + 0)
    exit
  }
  print "FOLDED\t0\t0"
}
