"""Corpus timing, same-session and interleaved
(`.agents/skills/slag-perf-measurement`).

Runs slag jit / slag jitless / node jit over the whole corpus in rounds with the
order reversed each round, keeps the min per row, checks every row's result
agrees across the columns, and prints the totals plus the widest rows.

Why interleaved rounds and not one sequential pass: a whole-corpus total moves
~15-25% between windows of the same session, and it moves BOTH engines — so a
single pass invites reading host load as an engine difference. `--rounds 5` is
the cheap minimum for a number worth writing down.

Usage:
    python tools/corpus/measure.py [--rounds N] [--slag PATH]

The binary defaults to `target/release/slag.exe` (`target/release/slag` off
Windows) and can be overridden with `--slag` or `$SLAG_BIN`.
"""

import argparse
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
WORKLOADS = HERE / "workloads"
RUNNER = HERE / "run_node.js"


def default_binary():
    suffix = ".exe" if os.name == "nt" else ""
    return ROOT / "target" / "release" / f"slag{suffix}"


def resolve_binary(arg):
    if arg:
        return Path(arg).resolve()
    env = os.environ.get("SLAG_BIN")
    if env:
        return Path(env).resolve()
    return default_binary()


def columns(slag):
    return {
        "slag-jit": [str(slag), "--corpus", str(WORKLOADS)],
        "slag-jitless": [str(slag), "--jitless", "--corpus", str(WORKLOADS)],
        "node-jit": ["node", str(RUNNER), str(WORKLOADS)],
    }


def run(cmd):
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=1800, check=False)
    if proc.returncode != 0:
        sys.exit(f"{cmd[0]} exited {proc.returncode}\n{proc.stderr}")
    rows = {}
    for line in proc.stdout.splitlines():
        parts = line.split("\t")
        if parts[0] != "bench" or len(parts) < 5:
            continue
        rows[parts[2]] = (float(parts[3]), parts[4])
    return rows


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--slag", help="path to the slag binary")
    args = parser.parse_args(argv)
    slag = resolve_binary(args.slag)
    if not slag.exists():
        sys.exit(f"measure: no binary at {slag} (build it, or pass --slag)")
    if not RUNNER.exists():
        sys.exit(f"measure: no node runner at {RUNNER}")

    runs = columns(slag)
    order = list(runs)
    best = {name: {} for name in runs}
    values = {name: {} for name in runs}
    for r in range(args.rounds):
        for name in order if r % 2 == 0 else list(reversed(order)):
            for row, (ms, value) in run(runs[name]).items():
                prev = best[name].get(row)
                if prev is None or ms < prev:
                    best[name][row] = ms
                values[name][row] = value

    names = sorted(best["slag-jit"])
    missing = [n for n in names if n not in best["node-jit"]]
    if missing:
        sys.exit(f"measure: rows missing on node: {missing[:5]}")

    differing = [
        n for n in names if len({values[c][n] for c in runs if n in values[c]}) > 1
    ]
    if differing:
        print(
            f"DIFFERENTIAL MISMATCH on {len(differing)} rows: {differing[:8]}",
            file=sys.stderr,
        )

    totals = {name: sum(best[name].values()) for name in runs}
    print(f"rows {len(names)}  rounds {args.rounds}  binary {slag}")
    for name in order:
        print(f"{name:14s} {totals[name]:9.1f} ms")
    print(f"{'slag/node':14s} {totals['slag-jit'] / totals['node-jit']:9.2f}x")
    print(f"{'jitless/jit':14s} {totals['slag-jitless'] / totals['slag-jit']:9.2f}x")
    print()

    print("row\tslag_jit\tnode_jit\tratio\tslag_jitless")
    widest = sorted(
        names,
        key=lambda n: best["slag-jit"][n] - best["node-jit"][n],
        reverse=True,
    )
    for n in widest:
        s, nd, jl = best["slag-jit"][n], best["node-jit"][n], best["slag-jitless"][n]
        print(f"{n}\t{s:.2f}\t{nd:.2f}\t{s / nd:.2f}\t{jl:.2f}")


if __name__ == "__main__":
    main()
