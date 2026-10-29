"""Rank the corpus for work, by absolute time, with the two discriminators the
ratio table was missing.

The mean-of-ratios headline gives every row the same weight, so a 0.6 ms row at
55x outranks a 400 ms row at 8.6x. This ranks by *recoverable time*
(`slag - node`), then adds:

  tier  - what `SLAG_OPT=0` (the baseline tier) does. A row the optimizing tier
          is slower on is a tier-selection bug, and that is visible in time here.
  jl    - the *interpreter* gap. Both engines interpret the same work, so V8's
          optimizer cannot flatter this column. `gap_jl` close to `gap_jit` means
          the deficit is in the runtime (allocation, closure creation, call
          dispatch) and is fixable there; `gap_jl` far below `gap_jit` means V8's
          optimizer is doing something we do not, and the row's ceiling may be
          unreachable.

Usage: python tools/corpus/where.py <bench.js-output> [tier.tsv]
  bench.js-output : stdout of `node tools/corpus/bench.js`
  tier.tsv        : `row<TAB>opt_ms<TAB>baseline_ms`, from interleaved corpus
                    runs with and without `SLAG_OPT=0`
"""

import re
import sys

ROW = re.compile(
    r"^(\S+\.js)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)\s+ok$"
)


def load_bench(path):
    rows = {}
    for line in open(path, encoding="utf-8", errors="replace"):
        m = ROW.match(line.replace("\r", "").strip())
        if not m:
            continue
        rows[m.group(1)] = {
            "slag": float(m.group(2)),
            "slag_jl": float(m.group(3)),
            "node": float(m.group(4)),
            "node_jl": float(m.group(5)),
        }
    return rows


def load_tier(path):
    out = {}
    for line in open(path, encoding="utf-8", errors="replace"):
        f = line.replace("\r", "").split("\t")
        if len(f) < 3:
            continue
        try:
            out[f[0]] = (float(f[1]), float(f[2]))
        except ValueError:
            continue
    return out


def ratio(a, b):
    return a / b if b > 0 else float("inf")


def verdict(gap_jit, gap_jl):
    """`runtime` when both tiers are equally off (shared machinery); `optimizer`
    when only compiled code is."""
    if gap_jl <= 0:
        return "?"
    if gap_jit / gap_jl >= 1.6:
        return "OPTIMIZER"
    if gap_jl >= 2.0:
        return "runtime"
    return "-"


def main(bench, tier_path=None):
    rows = load_bench(bench)
    tier = load_tier(tier_path) if tier_path else {}
    total = sum(r["slag"] for r in rows.values())
    print(f"{len(rows)} rows, corpus total {total:.1f} ms, work ranked by recoverable time\n")
    head = f"{'recover':>8} {'share':>6} {'slag':>8} {'node':>7} {'gap':>6} {'jl_gap':>6} {'tier':>7}  verdict / row"
    print(head)
    order = sorted(rows.items(), key=lambda kv: kv[1]["node"] - kv[1]["slag"])
    for name, r in order:
        rec = r["slag"] - r["node"]
        gap = ratio(r["slag"], r["node"])
        gjl = ratio(r["slag_jl"], r["node_jl"])
        delta = ""
        if name in tier:
            opt, base = tier[name]
            delta = f"{base - opt:+.2f}"
        v = verdict(gap, gjl)
        print(
            f"{rec:8.1f} {100 * r['slag'] / total:5.1f}% {r['slag']:8.2f} {r['node']:7.2f} "
            f"{gap:6.1f} {gjl:6.1f} {delta:>7}  {v:<9} {name}"
        )
    print("\nrows where the BASELINE tier is faster (a tier-selection bug, in ms):")
    losses = [
        (base - opt, name, opt, base)
        for name, (opt, base) in tier.items()
        if base < opt
    ]
    for delta, name, opt, base in sorted(losses):
        print(f"  {delta:+7.2f}  ({100 * delta / base:+.1f}%)  {name}")
    print(f"  total {sum(d for d, _, _, _ in losses):+.2f} ms")


args = sys.argv[1:]
if len(args) == 1:
    main(args[0])
else:
    main(args[0], args[1])
