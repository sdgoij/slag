"""The B0 materialization census probe (`.notes/register-tier-plan.md` §1.3).

Runs each corpus workload directly with `JIT_DUMP_IR` set, parses the census the
optimizing tier prints after the IR dump, and reports the worst body per row.
The conversions no materialization point explains (`compute`) are what Phase B
is worth; the two mechanism counts (`Op::Canon` sites, `Int` operand widens) are
exact, the five-way cause split is the census module's documented heuristic.

Usage:
    python tools/corpus/census_probe.py [--slag PATH] [rows...]

A row is a path under `workloads/`, e.g. `calls/method_call.js`; with no rows
every `*.js` in the corpus is probed. The binary defaults to
`target/release/slag.exe` (`target/release/slag` off Windows) and can be
overridden with `--slag` or `$SLAG_BIN`.
"""

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
WORKLOADS = HERE / "workloads"

CENSUS = re.compile(
    r"^(per iteration|per body \(no cycle\)): frame_store (\d+), frame_load (\d+), "
    r"canon (\d+), int->value (\d+) \((\d+) conversions\); causes: "
    r"frame (\d+), guard (\d+), alloc (\d+), call (\d+), compute (\d+), other (\d+); "
    r"avoidable (\d+)$"
)
BLOCKS = re.compile(r"^blocks (\d+), loops (\d+) \((\d+) cycle-resident blocks\)$")

COLUMNS = [
    "row",
    "bodies",
    "scope",
    "blocks",
    "loops",
    "conv",
    "avoid",
    "frame_store",
    "frame_load",
    "canon",
    "int_value",
    "compute",
    "call",
    "frame",
    "guard",
    "alloc",
    "other",
]


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


def run_workload(path, slag, extra_env=None, timeout=180):
    """Run one workload file directly; return the completed process."""
    env = dict(os.environ, JIT_DUMP_IR="1")
    if extra_env:
        env.update(extra_env)
    return subprocess.run(
        [str(slag), str(path)],
        env=env,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )


def census_blocks(stderr):
    """Every parsed census block in `stderr`, in emission order."""
    out = []
    lines = stderr.splitlines()
    for i, line in enumerate(lines):
        if line.strip() != "--- census ---":
            continue
        block = {"blocks": 0, "loops": 0, "cycle_blocks": 0}
        for follow in lines[i + 1 : i + 4]:
            follow = follow.strip()
            m = BLOCKS.match(follow)
            if m:
                block["blocks"] = int(m.group(1))
                block["loops"] = int(m.group(2))
                block["cycle_blocks"] = int(m.group(3))
            m = CENSUS.match(follow)
            if m:
                block.update(
                    scope=m.group(1),
                    frame_store=int(m.group(2)),
                    frame_load=int(m.group(3)),
                    canon=int(m.group(4)),
                    int_value=int(m.group(5)),
                    conversions=int(m.group(6)),
                    frame=int(m.group(7)),
                    guard=int(m.group(8)),
                    alloc=int(m.group(9)),
                    call=int(m.group(10)),
                    compute=int(m.group(11)),
                    other=int(m.group(12)),
                    avoidable=int(m.group(13)),
                )
        if "avoidable" in block:
            out.append(block)
    return out


def row_names(args):
    if args:
        return [Path(a) for a in args]
    return sorted(p.relative_to(WORKLOADS) for p in WORKLOADS.rglob("*.js"))


def label(rel):
    """A row's corpus-relative path, always with forward slashes."""
    return Path(rel).as_posix()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--slag", help="path to the slag binary")
    parser.add_argument("rows", nargs="*", help="rows under workloads/ (default: all)")
    args = parser.parse_args(argv)
    slag = resolve_binary(args.slag)
    if not slag.exists():
        sys.exit(f"census_probe: no binary at {slag} (build it, or pass --slag)")

    print("\t".join(COLUMNS))
    lifted = 0
    total = 0
    for rel in row_names(args.rows):
        total += 1
        name = label(rel)
        try:
            proc = run_workload(WORKLOADS / rel, slag)
        except subprocess.TimeoutExpired:
            print(f"{name}\tTIMEOUT")
            continue
        if proc.returncode != 0:
            print(f"{name}\tEXIT {proc.returncode}")
            continue
        found = census_blocks(proc.stderr)
        if not found:
            print(f"{name}\t0")
            continue
        lifted += 1
        worst = max(found, key=lambda b: (b["avoidable"], b["conversions"]))
        print(
            f"{name}\t{len(found)}\t{worst['scope']}\t{worst['blocks']}\t{worst['loops']}"
            f"\t{worst['conversions']}\t{worst['avoidable']}\t{worst['frame_store']}"
            f"\t{worst['frame_load']}\t{worst['canon']}\t{worst['int_value']}"
            f"\t{worst['compute']}\t{worst['call']}\t{worst['frame']}\t{worst['guard']}"
            f"\t{worst['alloc']}\t{worst['other']}"
        )
    print(f"# rows {total}, rows with a census {lifted}", file=sys.stderr)


if __name__ == "__main__":
    main()
