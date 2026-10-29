#!/usr/bin/env python3
"""Symbolize a `SLAG_SAMPLE` profile into a flat self-time table.

The sampler in `crates/cli/src/sampler.rs` prints image offsets and counts on
stderr, so the engine needs no symbol handling of its own. This tool turns
those offsets into functions, using two things the release build already
emits:

  * the PE unwind directory (`llvm-readobj --unwind`) gives every function's
    exact `[start, end)` range, including the ones with no public symbol;
  * the PDB's public symbols (`llvm-pdbutil dump -publics`) give the names.

A sampled address is attributed to the unwind range that contains it, and that
range is named by the public symbol at its start. Resolving the address
directly against the public list would let a nameless function's samples fall
through to whichever public symbol precedes it, which lumps megabytes of code
under one name.

Usage:
    SLAG_SAMPLE=300 ./target/release/slag.exe --corpus tools/corpus/workloads \\
        2> scratch/prof.txt
    python tools/profile.py scratch/prof.txt --group
    python tools/profile.py scratch/prof.txt --top 30

The symbol table is cached next to the binary and rebuilt when its size or
mtime changes, so the first run is slow (two dumps of a 100 MB image) and the
rest are instant.
"""

import argparse
import bisect
import json
import os
import shutil
import subprocess
import sys
from collections import defaultdict

# PE32+ images are linked at this base; addresses below are absolute VMAs.
IMAGE_BASE = 0x140000000


def llvm_tool(name):
    candidates = [
        os.path.join(os.path.dirname(os.environ["LLVM_SYMBOLIZER"]), name)
        if os.environ.get("LLVM_SYMBOLIZER")
        else None,
        shutil.which(name),
        os.path.join(r"C:\Program Files\LLVM\bin", name),
    ]
    for candidate in candidates:
        if candidate and os.path.exists(candidate):
            return candidate
    return None


def run(tool, args):
    result = subprocess.run(
        [tool] + args, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        print(f"{tool} failed: {result.stderr.strip()[:400]}", file=sys.stderr)
        raise SystemExit(1)
    return result.stdout


def text_rva(readobj, exe):
    out = run(readobj, ["--sections", exe])
    lines = out.split("\n")
    for index, line in enumerate(lines):
        if ".text" in line and "Name:" in line:
            for follow in lines[index : index + 8]:
                if "VirtualAddress" in follow:
                    return int(follow.split(":")[1].strip(), 16)
    return 0x1000


def build_symbols(exe):
    """[(start, end, name-or-None)] sorted by start, from the unwind + publics."""
    cache = exe + ".symbols.json"
    stamp = f"{os.path.getsize(exe)}:{int(os.path.getmtime(exe))}"
    if os.path.exists(cache):
        with open(cache, encoding="utf-8") as handle:
            cached = json.load(handle)
        if cached.get("stamp") == stamp:
            return cached["ranges"]

    readobj = llvm_tool("llvm-readobj.exe") or llvm_tool("llvm-readobj")
    pdbutil = llvm_tool("llvm-pdbutil.exe") or llvm_tool("llvm-pdbutil")
    cxxfilt = llvm_tool("llvm-cxxfilt.exe") or llvm_tool("llvm-cxxfilt")
    if not readobj or not pdbutil:
        print("llvm-readobj / llvm-pdbutil not found", file=sys.stderr)
        raise SystemExit(1)

    ranges = []
    start = None
    for line in run(readobj, ["--unwind", exe]).split("\n"):
        if "StartAddress:" in line:
            start = int(line.split("0x")[1].rstrip(") \r"), 16)
        elif "EndAddress:" in line and start is not None:
            end = int(line.split("0x")[1].rstrip(") \r"), 16)
            ranges.append([start, end])
            start = None
    ranges.sort()

    base = IMAGE_BASE + text_rva(readobj, exe)
    pdb = os.path.splitext(exe)[0] + ".pdb"
    mangled = []
    # `dump -publics` prints the name on one line and `addr = NNNN:OFFSET` on
    # the next, so the two are paired as they stream by.
    pending = None
    for line in run(pdbutil, ["dump", "-publics", pdb]).split("\n"):
        if "`" in line and "S_PUB32" in line:
            pending = line.split("`", 2)[1]
        elif pending is not None and "addr = " in line:
            offset = int(line.split("addr = ")[1].split()[0].split(":")[1])
            mangled.append((base + offset, pending))
            pending = None
    if cxxfilt and mangled:
        plain = subprocess.run(
            [cxxfilt],
            input="\n".join(name for _address, name in mangled) + "\n",
            capture_output=True,
            text=True,
            check=False,
        ).stdout.split("\n")
    else:
        plain = [name for _address, name in mangled]
    named = {}
    for (address, _name), readable in zip(mangled, plain):
        named.setdefault(address, readable.strip())

    table = []
    for begin, end in ranges:
        table.append([begin, end, named.get(begin)])
    # An anonymous range's samples still have to land somewhere: name it after
    # the nearest preceding named function, which is its neighbour in the same
    # object file (the linker keeps a translation unit's code together). The
    # label keeps the distinction visible.
    near = "unknown"
    for row in table:
        if row[2]:
            near = row[2]
        else:
            row[2] = "local near " + near
    with open(cache, "w", encoding="utf-8") as handle:
        json.dump({"stamp": stamp, "ranges": table}, handle)
    return table


class Symbols:
    def __init__(self, table):
        self.table = table
        self.starts = [row[0] for row in table]

    def name(self, address):
        index = bisect.bisect_right(self.starts, address) - 1
        if index < 0:
            return "unknown"
        begin, end, name = self.table[index]
        if address >= end:
            return "unknown"
        return name


def load_samples(path):
    """offset -> count, plus the total and the outside-image count."""
    offsets = defaultdict(int)
    declared = 0
    outside = 0
    for line in open(path, encoding="utf-8", errors="replace"):
        parts = line.replace("\r", "").split()
        if len(parts) < 3 or parts[0] != "profile":
            continue
        if parts[1] == "samples":
            if len(parts) >= 5:
                declared = int(parts[2])
                outside = int(parts[4])
            continue
        if parts[1].startswith("0x"):
            offsets[int(parts[1], 16)] += int(parts[2])
    total = declared or sum(offsets.values())
    return offsets, total, outside


def group_of(name):
    """The subsystem a symbolized function belongs to."""
    plain = name.split(" (.llvm")[0]
    if plain.startswith("local near "):
        plain = plain[len("local near ") :]
    if plain.startswith("_ZN") or plain.startswith("_$LT$"):
        return "unknown"
    parts = plain.split("::")
    if len(parts) >= 2:
        return "::".join(parts[:2])
    return parts[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profile", help="stderr of a SLAG_SAMPLE run")
    parser.add_argument("--obj", default="target/release/slag.exe")
    parser.add_argument("--top", type=int, default=40)
    parser.add_argument(
        "--group",
        action="store_true",
        help="aggregate by subsystem (crate::module) instead of by function",
    )
    args = parser.parse_args()

    offsets, total, outside = load_samples(args.profile)
    if total == 0:
        print("no samples in " + args.profile, file=sys.stderr)
        return 1
    symbols = Symbols(build_symbols(args.obj))

    by_function = defaultdict(int)
    if args.group:
        by_function["[jit/native code]"] += outside
    for offset, count in offsets.items():
        name = symbols.name(IMAGE_BASE + offset)
        by_function[group_of(name) if args.group else name] += count

    print(f"samples {total}  outside-image (JIT) {outside} ({100 * outside / total:.1f}%)")
    print(f"{'share':>7} {'samples':>8}  {'subsystem' if args.group else 'function'}")
    rows = sorted(by_function.items(), key=lambda row: -row[1])
    for name, count in rows[: args.top]:
        print(f"{100 * count / total:6.2f}% {count:8d}  {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
