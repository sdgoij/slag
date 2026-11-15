#!/usr/bin/env python3
"""Per-row helper traffic: the JIT_HELPER_STATS histogram, one row per process.

`JIT_HELPER_STATS=1` emits one inline increment per helper call site into a
process-wide array, and the CLI prints the histogram at the end. One workload
per process is therefore the only way to attribute traffic to a row.

Usage:
    python tools/corpus/helper_rows.py [--slag PATH] [--top N] [--helper IDX]

Prints, per row, the total helper calls and the row's largest helper indices.
`--helper IDX` prints the rows that call one helper, largest first (the index is
the 0-based `Helper` variant order in `crates/jit/src/helpers.rs`).
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKLOADS = HERE / "workloads"
ROOT = HERE.parent.parent

# The indices this probe is normally pointed at; keep the names handy so the
# output reads without opening helpers.rs.
NAMES = {
    0: "BinarySlow",
    1: "UnarySlow",
    8: "ToBooleanSlow",
    11: "GetMemberName",
    14: "SetMemberName",
    18: "CallSlow",
    19: "LeafCallProbe",
    20: "LeafCallFill",
    22: "CertifiedCall",
    26: "LoadIdent",
    48: "CreateFunction",
    70: "ObjectBegin",
    99: "ForInNext",
    123: "TypeofTop",
}


def run(binary, directory):
    env = dict(os.environ, JIT_HELPER_STATS="1")
    proc = subprocess.run(
        [binary, "--corpus", directory],
        capture_output=True,
        text=True,
        timeout=900,
        env=env,
        check=False,
    )
    if proc.returncode != 0:
        return None
    counts = {}
    for line in proc.stdout.splitlines():
        parts = line.split("\t")
        if len(parts) == 3 and parts[0] == "helper":
            counts[int(parts[1])] = int(parts[2])
    return counts


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--slag", default=str(ROOT / "target" / "release" / "slag.exe"))
    ap.add_argument("--top", type=int, default=3)
    ap.add_argument("--helper", type=int, default=None)
    ap.add_argument("--only", default="")
    args = ap.parse_args(argv)
    if not Path(args.slag).exists():
        sys.exit(f"helper_rows: no binary at {args.slag}")
    rows = []
    for path in sorted(p for p in WORKLOADS.rglob("*.js") if args.only in str(p)):
        tmp = Path(tempfile.mkdtemp(prefix="helpers-"))
        try:
            shutil.copy(path, tmp / path.name)
            counts = run(args.slag, str(tmp))
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
        if counts is None:
            continue
        rel = path.relative_to(WORKLOADS).as_posix()
        rows.append((rel, counts))
    if args.helper is not None:
        idx = args.helper
        picked = [(c.get(idx, 0), rel) for rel, c in rows]
        picked.sort(reverse=True)
        for n, rel in picked:
            if n:
                print(f"{n:12d}  {rel}")
        print(f"# {NAMES.get(idx, idx)} total {sum(n for n, _ in picked)}")
        return
    for rel, counts in sorted(rows, key=lambda r: -sum(r[1].values())):
        top = sorted(counts.items(), key=lambda kv: -kv[1])[: args.top]
        detail = "  ".join(
            f"{NAMES.get(i, i)}={n:,}" for i, n in top if n
        )
        print(f"{sum(counts.values()):12,d}  {rel:58s} {detail}")


if __name__ == "__main__":
    main()
