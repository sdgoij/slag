#!/usr/bin/env python3
"""Surface ledger for the quickjs C facade.

Cross-checks the facade's exported `JS_*`/`js_*` symbols against the selected
vendored header, and against a per-feature `unsupported.<feature>.toml` gap
list. Three failures are reported:

- unexpected: an export whose name does not appear anywhere in the header (a
  typo, or an accidental `#[no_mangle]` leak).
- missing: a header link-symbol declaration (`JS_EXTERN` for ng; a non-static,
  non-typedef function declaration for bellard) that is neither exported nor
  listed in `unsupported.<feature>.toml`.
- stale: an `unsupported` entry for a symbol that is now exported.

Run once per feature (they are mutually exclusive, so each has its own header
and gap list): `tools/quickjs-surface.py ng` / `tools/quickjs-surface.py bellard`.

`--bless` (re)writes the gap list for a feature from the current
header-minus-exports delta, so new gaps are recorded rather than reported.
"""
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HELPERS = {"qjs_new_error", "qjs_throw_error"}


def rust_exports(feature: str) -> set[str]:
    names: set[str] = set()
    pattern = re.compile(
        r'(?:#\[cfg\(feature\s*=\s*"(ng|bellard)"\)\]\s*)?'
        r'#\[(?:unsafe\()?no_mangle\)?\]\s*'
        r'(?:pub\s+)?unsafe\s+extern\s+"C"\s+fn\s+([A-Za-z_]\w*)'
    )
    for path in (ROOT / "crates/quickjs/src/c").glob("*.rs"):
        if feature == "ng" and path.name == "bellard.rs":
            continue
        for gate, name in pattern.findall(path.read_text(encoding="utf-8")):
            if gate and gate != feature:
                continue
            names.add(name)
    return names


def shim_exports(feature: str) -> set[str]:
    text = (ROOT / "crates/quickjs/c/shim/variadic.c").read_text(encoding="utf-8")
    names: set[str] = set()
    ng_only: set[str] = set()
    in_ng = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("#ifdef QJS_NG"):
            in_ng = True
            continue
        if stripped.startswith("#endif"):
            in_ng = False
            continue
        for name in re.findall(r"DEFINE_(?:NEW|THROW)_ERROR\((JS_\w+)", stripped):
            names.add(name)
            if in_ng:
                ng_only.add(name)
        for name in re.findall(r"\bJSValue\s+(JS_ThrowDOMException)\s*\(", stripped):
            names.add(name)
            if in_ng:
                ng_only.add(name)
    if feature != "ng":
        names -= ng_only
    return names


def c_test_symbols() -> set[str]:
    """External-linkage function symbols defined by the C test/link harness."""
    symbols: set[str] = set()
    definition = re.compile(
        r"(?m)(?P<pre>^[\s\w\*]*)\b(?P<name>[A-Za-z_]\w*)\s*\([^;{}]*\)\s*\{"
    )
    for directory in ("ng", "bellard"):
        for path in (ROOT / f"crates/quickjs/c/tests/{directory}").glob("*.c"):
            for match in definition.finditer(path.read_text(encoding="utf-8")):
                if "static" in match.group("pre"):
                    continue
                symbols.add(match.group("name"))
    return symbols


def header_text(feature: str) -> str:
    text = (ROOT / f"crates/quickjs/c/include/{feature}/quickjs.h").read_text(
        encoding="utf-8"
    )
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


def header_mentioned(feature: str) -> set[str]:
    # Every JS_/js_ identifier the header names (inline helpers, macros,
    # declarations). A superset of legitimate export names.
    return set(re.findall(r"\b((?:JS|js)_[A-Za-z0-9_]+)\b", header_text(feature)))


def header_contract(feature: str) -> set[str]:
    text = header_text(feature)
    names: set[str] = set()
    if feature == "ng":
        for match in re.finditer(r"(?m)^[ \t]*JS_EXTERN\b", text):
            start = match.end()
            end = text.find(";", start)
            if end == -1:
                end = len(text)
            decl = " ".join(text[start:end].split())
            names.update(re.findall(r"\b((?:JS|js)_[A-Za-z0-9_]+)\s*\(", decl))
    else:
        # Bellard declares its link symbols without an export macro. Match
        # function prototypes (`NAME(...);`). `[^();]*` keeps the parameter list
        # from crossing a nested `(`, so macro invocations and inline bodies
        # (whose bodies use nested calls or end in `{`) are not mistaken for
        # declarations.
        names.update(
            re.findall(r"\b((?:JS|js)_[A-Za-z0-9_]+)\s*\([^();]*\)\s*;", text)
        )
    return names


def load_unsupported(feature: str) -> set[str]:
    path = ROOT / f"crates/quickjs/c/unsupported.{feature}.toml"
    if not path.exists():
        return set()
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    return set(data.get("unsupported", []))


def bless(feature: str) -> None:
    gap = header_contract(feature) - (rust_exports(feature) | shim_exports(feature))
    path = ROOT / f"crates/quickjs/c/unsupported.{feature}.toml"
    lines = [
        f"# Header link-symbol declarations the {feature} skin does not implement.\n",
        "# Regenerate with: tools/quickjs-surface.py --bless {feature}\n".replace(
            "{feature}", feature
        ),
        "unsupported = [\n",
    ]
    lines += [f'    "{name}",\n' for name in sorted(gap)]
    lines.append("]\n")
    path.write_text("".join(lines), encoding="utf-8", newline="\n")
    print(f"wrote {path} ({len(gap)} entries)")


def main() -> None:
    args = [a for a in sys.argv[1:]]
    if "--bless" in args:
        args.remove("--bless")
        if not args or args[0] not in ("ng", "bellard"):
            print("usage: quickjs-surface.py [--bless] ng|bellard", file=sys.stderr)
            sys.exit(2)
        bless(args[0])
        return

    if not args or args[0] not in ("ng", "bellard"):
        print("usage: quickjs-surface.py [--bless] ng|bellard", file=sys.stderr)
        sys.exit(2)
    feature = args[0]

    exports = rust_exports(feature) | shim_exports(feature)
    mentioned = header_mentioned(feature)
    contract = header_contract(feature)
    unsupported = load_unsupported(feature)

    unexpected = (exports - mentioned) - HELPERS
    missing = (contract - exports) - unsupported
    stale = unsupported & exports
    collisions = exports & c_test_symbols()

    if unexpected:
        print(f"unexpected exports (not in the {feature} header):")
        for name in sorted(unexpected):
            print(f"  {name}")
    if missing:
        print(f"missing exports (declared, not exported, not in unsupported):")
        for name in sorted(missing):
            print(f"  {name}")
    if stale:
        print("stale unsupported entries (now exported):")
        for name in sorted(stale):
            print(f"  {name}")
    if collisions:
        print("symbol collisions (an export redefined by the C test harness):")
        for name in sorted(collisions):
            print(f"  {name}")

    status = "OK" if not (unexpected or missing or stale or collisions) else "FAIL"
    print(
        f"surface ledger [{feature}]: exports={len(exports)} contract={len(contract)} "
        f"unsupported={len(unsupported)} unexpected={len(unexpected)} "
        f"missing={len(missing)} stale={len(stale)} collisions={len(collisions)} -> {status}"
    )
    sys.exit(0 if status == "OK" else 1)


if __name__ == "__main__":
    main()
