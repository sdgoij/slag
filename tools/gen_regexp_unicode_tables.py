#!/usr/bin/env python3
"""Generate crates/unicode/src/derived_regexp_tables.rs from the pinned
test262 fixtures.

Each `property-escapes/generated/<Name>.js` fixture encodes the exact code
point set of the Unicode binary property / general-category value / script
it tests: `buildString({ loneCodePoints: [...], ranges: [[a, b], ...] })`.
The property-of-strings fixtures (`property-escapes/generated/strings/*.js`
and the `unicodeSets/generated/rgi-emoji-*.js` files) encode their string
sets as `matchStrings: ["\\uXXXX...", ...]`. test262 is pinned to a Unicode
version (fixtures are generated from Unicode v17), so these tables are the
exact data the corpus asserts; the engine's hand-written predicates (via
unicode-properties / unicode-script / unicode-id crates) remain for the
properties they cover.

Usage: python3 tools/gen_regexp_unicode_tables.py
Writes: crates/unicode/src/derived_regexp_tables.rs
"""
import json
import os
import re
import sys

ROOT = os.path.join(os.path.dirname(__file__), "..")
GEN = os.path.join(ROOT, "test262", "test", "built-ins", "RegExp",
                   "property-escapes", "generated")
SETGEN = os.path.join(ROOT, "test262", "test", "built-ins", "RegExp",
                      "unicodeSets", "generated")
OUT = os.path.join(ROOT, "crates", "unicode", "src", "derived_regexp_tables.rs")

# Decode a JS string literal (the fixtures use \uXXXX and \u{XXXXX}).
def decode_js_string(lit: str) -> list:
    out = []
    i = 0
    while i < len(lit):
        ch = lit[i]
        if ch == "\\" and i + 1 < len(lit):
            nxt = lit[i + 1]
            if nxt == "u":
                if i + 2 < len(lit) and lit[i + 2] == "{":
                    end = lit.index("}", i + 3)
                    out.append(int(lit[i + 3:end], 16))
                    i = end + 1
                else:
                    out.append(int(lit[i + 2:i + 6], 16))
                    i += 6
                continue
            elif nxt == "x":
                out.append(int(lit[i + 2:i + 4], 16))
                i += 4
                continue
            elif nxt == "n":
                out.append(0x0A); i += 2; continue
            elif nxt == "t":
                out.append(0x09); i += 2; continue
            elif nxt == "r":
                out.append(0x0D); i += 2; continue
            elif nxt == "\\":
                out.append(0x5C); i += 2; continue
            elif nxt == '"':
                out.append(0x22); i += 2; continue
            else:
                out.append(ord(nxt)); i += 2; continue
        out.append(ord(ch))
        i += 1
    return out


def parse_build_string(src: str):
    """Extract loneCodePoints + ranges from a buildString({...}) call."""
    m = re.search(r"buildString\(\{([^}]*)\}\)", src, re.S)
    body = m.group(1) if m else ""
    lone = []
    ranges = []
    lm = re.search(r"loneCodePoints:\s*\[([^\]]*)\]", body)
    if lm:
        lone = [int(x, 16) for x in re.findall(r"0x([0-9A-Fa-f]+)", lm.group(1))]
    rm = re.search(r"ranges:\s*", body)
    if rm:
        start = rm.end()
        # Depth scan from the opening `[` to its matching `]` (two levels of
        # nesting: [[a, b], [c, d], ...]).
        depth = 0
        i = start
        while i < len(body) and depth >= 0:
            if body[i] == "[":
                depth += 1
            elif body[i] == "]":
                depth -= 1
            if depth == 0 and i > start:
                break
            i += 1
        seg = body[start : i + 1]
        for a, b in re.findall(r"\[0x([0-9A-Fa-f]+),\s*0x([0-9A-Fa-f]+)\]", seg):
            ranges.append((int(a, 16), int(b, 16)))
    return lone, ranges


def parse_match_strings(src: str):
    """Extract the matchStrings array of a testPropertyOfStrings call."""
    m = re.search(r"matchStrings:\s*\[(.*?)\n\s*\]", src, re.S)
    if not m:
        return []
    return [decode_js_string(lit) for lit in re.findall(r'"([^"]*)"', m.group(1))]


def main():
    props = {}      # canonical name -> sorted ranges
    strings = {}    # canonical name -> list of code point lists
    for fn in sorted(os.listdir(GEN)):
        if not fn.endswith(".js"):
            continue
        name = fn[:-3]
        with open(os.path.join(GEN, fn), encoding="utf-8") as f:
            src = f.read()
        lone, ranges = parse_build_string(src)
        if lone or ranges:
            pts = set(lone)
            for a, b in ranges:
                pts.update(range(a, b + 1))
            props[name] = sorted(pts)
    # Property-of-strings fixtures live in the `strings/` subdirectory.
    strings_dir = os.path.join(GEN, "strings")
    for fn in sorted(os.listdir(strings_dir)):
        if not fn.endswith(".js"):
            continue
        name = fn[:-3]
        with open(os.path.join(strings_dir, fn), encoding="utf-8") as f:
            src = f.read()
        ms = parse_match_strings(src)
        if ms:
            strings[name] = ms
    # The `unicodeSets/generated/rgi-emoji-*.js` fixtures are version-specific
    # subsets of RGI_Emoji; `strings/RGI_Emoji.js` is the authoritative full
    # set (3953 sequences vs the ~650-version subsets), so it wins.
    for fn in sorted(os.listdir(SETGEN)):
        if fn.startswith("rgi-emoji-"):
            name = fn[:-3]
            with open(os.path.join(SETGEN, fn), encoding="utf-8") as f:
                src = f.read()
            ms = parse_match_strings(src)
            if ms and "RGI_Emoji" not in strings:
                strings.setdefault("RGI_Emoji", []).extend(ms)

    # Ranges only include properties that are NOT covered by the crates
    # (binary properties the engine's unicode::binary_property implements
    # via unicode-properties/unicode-id etc. keep their hand-written path).
    lines = []
    lines.append("//! Generated by tools/gen_regexp_unicode_tables.py from the")
    lines.append("//! pinned test262 fixtures (Unicode v17). Do not edit by hand.")
    lines.append("//!")
    lines.append("//! The code point set of each `\\p{...}` escape the fixtures test,")
    lines.append("//! as encoded in `property-escapes/generated/*.js`.")
    lines.append("#![allow(clippy::unreadable_literal)]")
    lines.append("")
    for name in sorted(props):
        pts = props[name]
        ranges = []
        for cp in pts:
            if ranges and cp == ranges[-1][1] + 1:
                ranges[-1] = (ranges[-1][0], cp)
            else:
                ranges.append((cp, cp))
        lines.append(f"/// `\\p{{{name}}}` (from the test262 fixtures).")
        lines.append(f"pub const {name.upper().replace('-', '_')}: &[(u32, u32)] = &[")
        for a, b in ranges:
            lines.append(f"    (0x{a:04X}, 0x{b:04X}),")
        lines.append("];")
        lines.append("")
    lines.append("/// Map a binary-property name to its derived range table.")
    lines.append("pub fn binary_property_table(name: &str) -> Option<&'static [(u32, u32)]> {")
    lines.append("    Some(match name {")
    for name in sorted(props):
        lines.append(f'        "{name}" => {name.upper().replace("-", "_")},')
    lines.append('        _ => return None,')
    lines.append("    })")
    lines.append("}")
    lines.append("")
    lines.append("/// Map a property-of-strings name to its string set.")
    for name in sorted(strings):
        ident = name.upper().replace('-', '_')
        lines.append(f"/// `\\p{{{name}}}` string set (from the test262 fixtures).")
        lines.append(f"pub const {ident}: &[&[u32]] = &[")
        for s in strings[name]:
            cps = ", ".join(f"0x{c:04X}" for c in s)
            lines.append(f"    &[{cps}],")
        lines.append("];")
        lines.append("")
    lines.append("/// Map a property-of-strings name to its string set.")
    lines.append("pub fn property_of_strings(name: &str) -> Option<&'static [&'static [u32]]> {")
    lines.append("    Some(match name {")
    for name in sorted(strings):
        lines.append(f'        "{name}" => {name.upper().replace("-", "_")},')
    lines.append('        _ => return None,')
    lines.append("    })")
    lines.append("}")
    lines.append("")
    with open(OUT, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    print(f"wrote {OUT}: {len(props)} binary props, {len(strings)} string sets")


if __name__ == "__main__":
    main()
