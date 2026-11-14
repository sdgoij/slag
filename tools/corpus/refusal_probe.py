"""Why the rows with no census have no census: the lift/opt bail reasons.

`JIT_DUMP_CLIF` prints `opt bail: <reason> (<n> steps)` once per refused body;
`JIT_DUMP_STEPS` adds the refusal's step and the body's step shapes. This
reports, per row, the bail reasons and the step-shape classes, so a coverage
bucket in `.notes/register-tier-plan.md` §3 is measured rather than remembered.

Usage:
    python tools/corpus/refusal_probe.py [--slag PATH] [all|no-census ...]

The default (`no-census`) first runs `census_probe` and reports only the rows
that produce no census. `all` reports every row. Explicit row paths skip the
detection pass.

Two readings of the output:
  - `opt-blockers` non-empty means a *step* is outside the lift's subset, and it
    names the variants.
  - `opt-blockers` empty with a `Step(...)` bail means the refusal is a *shape*
    inside a step the lift models; `opt-refuse:` names the step when the lift
    saw it (a refused terminator names its family, e.g. `HoistGuard`).
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

# The sibling import below needs this even when the interpreter is started with
# PYTHONSAFEPATH (which omits the script's own directory from `sys.path`).
_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from census_probe import (  # noqa: E402
    WORKLOADS,
    census_blocks,
    label,
    resolve_binary,
    row_names,
    run_workload,
)

BAIL = re.compile(r"opt bail: (.+?) \(\d+ steps\)")
BLOCKERS = re.compile(r"^opt-blockers:(.*)$")
SHAPES = re.compile(r"^opt-shapes:(.*)$")
REFUSE = re.compile(r"^opt-refuse: (.+)$")


def probe_row(rel, slag):
    proc = run_workload(
        WORKLOADS / rel, slag, extra_env={"JIT_DUMP_CLIF": "1", "JIT_DUMP_STEPS": "1"}
    )
    bails, blockers, shapes, refuses = [], set(), set(), []
    for line in proc.stderr.splitlines():
        line = line.strip()
        if m := BAIL.search(line):
            bails.append(m.group(1))
        elif m := BLOCKERS.match(line):
            blockers.update(n for n in m.group(1).strip().split(",") if n)
        elif m := SHAPES.match(line):
            shapes.update(n for n in m.group(1).strip().split(",") if n)
        elif m := REFUSE.match(line):
            refuses.append(m.group(1))
    return proc, bails, sorted(blockers), sorted(shapes), refuses


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--slag", help="path to the slag binary")
    parser.add_argument(
        "selection",
        nargs="*",
        default=["no-census"],
        help="'all', 'no-census', or explicit rows",
    )
    args = parser.parse_args(argv)
    slag = resolve_binary(args.slag)
    if not slag.exists():
        sys.exit(f"refusal_probe: no binary at {slag} (build it, or pass --slag)")

    selection = args.selection
    if selection == ["no-census"]:
        rows = []
        for rel in row_names([]):
            try:
                proc = run_workload(WORKLOADS / rel, slag)
            except subprocess.TimeoutExpired:
                continue
            if proc.returncode == 0 and not census_blocks(proc.stderr):
                rows.append(rel)
        print(f"# {len(rows)} rows without a census", file=sys.stderr)
    elif selection == ["all"]:
        rows = row_names([])
    else:
        rows = [Path(s) for s in selection]

    for rel in rows:
        proc, bails, blockers, shapes, refuses = probe_row(rel, slag)
        reason = " | ".join(sorted(set(bails))) or "<no opt bail>"
        print(f"{label(rel)}\t{reason}")
        if blockers:
            print(f"  blockers: {', '.join(blockers)}")
        if refuses:
            for r in refuses:
                print(f"  refusals: {r}")
        if shapes:
            print(f"  shapes: {', '.join(shapes)}")
        if proc.returncode != 0:
            print(f"  exit {proc.returncode}")


if __name__ == "__main__":
    main()
