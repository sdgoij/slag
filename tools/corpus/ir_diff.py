"""Which rows' generated IR differs between two slag binaries.

A tier change can only move a row whose IR (or lowering verdict) changed. Every
other row's cross-binary timing delta is native layout, and this corpus has
measured those swings at 30-40% on rows the change cannot reach — so this probe
is the first thing to run before believing any row-level number from an A/B.

Runs each row in a private directory with `JIT_DUMP_IR=1`, keeps only the IR
blocks (the census counts are derived from the same IR, and the bail lines are
`refusal_probe.py`'s question), strips heap addresses (they differ per process
for reasons unrelated to the change), and compares.

Usage:
    python tools/corpus/ir_diff.py CONTROL VARIANT [--only SUBSTR]

Both paths are binaries; `--only` limits the rows to those whose relative path
contains the substring.
"""

import argparse
import os
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKLOADS = HERE / "workloads"
HEX = re.compile(r"0x[0-9a-fA-F]+")


def ir(binary, directory):
    """The row's dumped IR blocks, hex-normalized."""
    env = dict(os.environ, JIT_DUMP_IR="1")
    proc = subprocess.run(
        [binary, "--corpus", directory],
        capture_output=True,
        text=True,
        timeout=600,
        env=env,
        check=False,
    )
    if proc.returncode != 0:
        raise SystemExit(f"{binary} exited {proc.returncode}: {proc.stderr[-2000:]}")
    kept = []
    in_ir = False
    for line in proc.stderr.splitlines():
        if line.startswith("--- IR ---"):
            in_ir = True
            continue
        if line.startswith("--- census ---"):
            in_ir = False
            continue
        if in_ir:
            kept.append(HEX.sub("0x?", line))
    return "\n".join(kept)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("control", help="the reference binary")
    parser.add_argument("variant", help="the binary under test")
    parser.add_argument("--only", default="", help="only rows whose path contains this")
    args = parser.parse_args(argv)
    for path in (args.control, args.variant):
        if not Path(path).exists():
            raise SystemExit(f"ir_diff: no binary at {path}")
    files = sorted(p for p in WORKLOADS.rglob("*.js") if args.only in str(p))
    changed = []
    for path in files:
        tmp = Path(tempfile.mkdtemp(prefix="ir-diff-"))
        try:
            shutil.copy(path, tmp / path.name)
            control = ir(args.control, str(tmp))
            variant = ir(args.variant, str(tmp))
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
        if control != variant:
            changed.append(path.relative_to(WORKLOADS).as_posix())
    print(f"{len(changed)} of {len(files)} rows have different IR")
    for row in changed:
        print(row)


if __name__ == "__main__":
    main()
