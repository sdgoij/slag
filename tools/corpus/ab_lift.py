"""A/B one row's optimizing tier against its per-step tier (`SLAG_OPT=0`).

The gate for a lift cut is "entering the tier is never a pessimization"
(`.notes/register-tier-plan.md` §4): one workload per process (`--corpus <dir>`,
the one-row-per-process protocol), rounds with the order reversed, min per row,
and the two columns' results compared so a miscompile cannot hide behind a
timing.

Defaults to the rows a hoist-guard lift would newly cover; pass rows to check
others.

Usage:
    python tools/corpus/ab_lift.py [--rounds N] [--slag PATH] [rows...]

The binary defaults to `target/release/slag.exe` (`target/release/slag` off
Windows) and can be overridden with `--slag` or `$SLAG_BIN`.
"""

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
WORKLOADS = HERE / "workloads"
STAGE = ROOT / "target" / "ab-rows"

DEFAULT_ROWS = [
    "globals/declarative_read.js",
    "globals/nested_read.js",
    "globals/object_read.js",
    "objects/own_read.js",
    "objects/proto_read.js",
    "opcost/obj_prop.js",
]


def resolve_binary(arg):
    if arg:
        return Path(arg).resolve()
    env = os.environ.get("SLAG_BIN")
    if env:
        return Path(env).resolve()
    return ROOT / "target" / "release" / ("slag.exe" if os.name == "nt" else "slag")


def stage(row, slag):
    """A one-workload directory for `--corpus`, keyed by the binary too."""
    d = STAGE / row.replace("/", "_").removesuffix(".js")
    d.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(WORKLOADS / row, d / "workload.js")
    return d


def run(d, slag, opt):
    env = dict(os.environ)
    if opt:
        env.pop("SLAG_OPT", None)
    else:
        env["SLAG_OPT"] = "0"
    proc = subprocess.run(
        [str(slag), "--corpus", str(d)],
        env=env,
        capture_output=True,
        text=True,
        timeout=1800,
        check=False,
    )
    if proc.returncode != 0:
        sys.exit(f"--corpus {d} exited {proc.returncode}\n{proc.stderr}")
    for line in proc.stdout.splitlines():
        parts = line.split("\t")
        if parts[0] == "bench":
            return float(parts[3]), parts[4]
    sys.exit(f"no bench line from {d}\n{proc.stdout}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--slag", help="path to the slag binary")
    parser.add_argument("rows", nargs="*", help="rows under workloads/")
    args = parser.parse_args(argv)
    slag = resolve_binary(args.slag)
    if not slag.exists():
        sys.exit(f"ab_lift: no binary at {slag} (build it, or pass --slag)")
    rows = args.rows or DEFAULT_ROWS

    print("row\topt_ms\tper_step_ms\topt/per_step\tresult_opt\tresult_step")
    for row in rows:
        d = stage(row, slag)
        best = {True: None, False: None}
        values = {}
        for r in range(args.rounds):
            for opt in (True, False) if r % 2 == 0 else (False, True):
                ms, value = run(d, slag, opt)
                values[opt] = value
                if best[opt] is None or ms < best[opt]:
                    best[opt] = ms
        ratio = best[True] / best[False] if best[False] else float("inf")
        print(
            f"{row}\t{best[True]:.2f}\t{best[False]:.2f}\t{ratio:.3f}"
            f"\t{values[True]}\t{values[False]}"
        )


if __name__ == "__main__":
    main()
