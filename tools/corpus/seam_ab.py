#!/usr/bin/env python3
"""Same-binary A/B for an env-gated pass seam (no cross-binary layout).

Runs each row in a private directory twice per round — with the seam's env var
unset and with it set to `0` — alternating the order, and reports the min per
side. Because both sides are the same executable, the delta is the pass's
marginal effect with no layout, provenance or rebuild in it, which is the only
protocol that held up for rows under ~5 ms (see the skill's "same-binary seam").

Usage: python tools/corpus/seam_ab.py BINARY ENV_VAR ROW [ROW ...] [--rounds 9]
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
WORKLOADS = ROOT / "tools" / "corpus" / "workloads"


def run(binary, directory, env):
    proc = subprocess.run(
        [binary, "--corpus", directory],
        capture_output=True,
        text=True,
        timeout=1800,
        env=env,
        check=False,
    )
    if proc.returncode != 0:
        sys.exit(f"{binary} exited {proc.returncode}\n{proc.stderr[-2000:]}")
    for line in proc.stdout.splitlines():
        p = line.split("\t")
        if len(p) >= 5 and p[0] == "bench":
            return float(p[3]), p[4]
    sys.exit(f"no bench row for {directory}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("var")
    ap.add_argument("rows", nargs="+")
    ap.add_argument("--rounds", type=int, default=9)
    args = ap.parse_args()
    on = dict(os.environ)
    off = dict(os.environ, **{args.var: "0"})
    print(f"{'row':56s} {'on':>9s} {'off':>9s} {'delta%':>7s}")
    for rel in args.rows:
        src = WORKLOADS / rel
        tmp = Path(tempfile.mkdtemp(prefix="seam-"))
        try:
            shutil.copy(src, tmp / src.name)
            best = {"on": None, "off": None}
            values = {"on": None, "off": None}
            for r in range(args.rounds):
                order = [("on", on), ("off", off)]
                if r % 2:
                    order.reverse()
                for tag, env in order:
                    ms, value = run(args.binary, str(tmp), env)
                    best[tag] = ms if best[tag] is None else min(best[tag], ms)
                    values[tag] = value
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
        a, b = best["on"], best["off"]
        note = "" if values["on"] == values["off"] else "  VALUE MISMATCH"
        print(f"{rel:56s} {a:9.3f} {b:9.3f} {100.0 * (b - a) / a:7.1f}{note}")


if __name__ == "__main__":
    main()
