//! Generate `derived_regexp_tables.rs` from the pinned test262 submodule.
//!
//! The RegExp property-escapes fixtures encode the exact code point set of
//! every binary property / general-category value / script the corpus tests
//! (`buildString({ loneCodePoints: [...], ranges: [[a, b], ...] })`), and the
//! `strings/` fixtures encode the property-of-strings sets (`matchStrings`).
//! The fixtures are pinned with the test262 submodule, so generating the
//! tables from them at compile time means they can never drift from the
//! corpus. The submodule must be checked out; the build fails with
//! instructions otherwise, because these tables are load-bearing (grapheme
//! segmentation and `\p{...}` lookups read them).

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Relative to `CARGO_MANIFEST_DIR` (the crate root).
const FIXTURES_REL: &str = "../../test262/test/built-ins/RegExp/property-escapes/generated";

fn main() {
    if let Err(msg) = run() {
        eprintln!("error: {msg}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?);
    let fixtures = manifest.join(FIXTURES_REL);
    let strings = fixtures.join("strings");

    if !fixtures.is_dir() || !strings.is_dir() {
        return Err(format!(
            "the test262 submodule is not checked out; crates/unicode needs the pinned \
             RegExp property-escapes fixtures to generate its `\\p{{...}}` tables\n\
             expected: {}\n\
             fix: run `git submodule update --init test262` from the repository root, then rebuild",
            fixtures.display()
        ));
    }
    println!("cargo:rerun-if-changed={}", fixtures.display());
    println!("cargo:rerun-if-changed={}", strings.display());

    let mut props: BTreeMap<String, Vec<(u32, u32)>> = BTreeMap::new();
    for path in js_files(&fixtures)? {
        let src = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let (lone, ranges) = parse_build_string(&src);
        if lone.is_empty() && ranges.is_empty() {
            continue;
        }
        props.insert(file_stem(&path), merge_ranges(lone, ranges));
    }

    let mut string_sets: BTreeMap<String, Vec<Vec<u32>>> = BTreeMap::new();
    for path in js_files(&strings)? {
        let src = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let seqs = parse_match_strings(&src);
        if !seqs.is_empty() {
            string_sets.insert(file_stem(&path), seqs);
        }
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|e| e.to_string())?);
    let out_path = out_dir.join("derived_regexp_tables.rs");
    fs::write(&out_path, render(&props, &string_sets)).map_err(|e| e.to_string())?;
    Ok(())
}

fn js_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|ext| ext == "js") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .expect("fixture file names are ASCII")
        .to_string()
}

/// Union lone code points and ranges into sorted, coalesced ranges (adjacent
/// and overlapping ranges merge). Equivalent to the generator's point-set
/// expansion, without materializing every point (`Any` alone is ~1.1M).
fn merge_ranges(lone: Vec<u32>, ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    let mut all: Vec<(u32, u32)> = lone.into_iter().map(|cp| (cp, cp)).chain(ranges).collect();
    all.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (a, b) in all {
        match merged.last_mut() {
            Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

/// Return the text between the first `open` bracket and its matching `close`
/// bracket that follow `needle` in `src` (the brackets are not included).
fn bracket_body<'a>(src: &'a str, needle: &str, open: u8, close: u8) -> Option<&'a str> {
    let idx = src.find(needle)?;
    let rest = &src[idx + needle.len()..];
    let bytes = rest.as_bytes();
    let mut depth = 0u32;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b if b == open => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            b if b == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[start + 1..i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Parse a `buildString({ loneCodePoints: […], ranges: [[a, b], …] })` call.
fn parse_build_string(src: &str) -> (Vec<u32>, Vec<(u32, u32)>) {
    let Some(body) = bracket_body(src, "buildString(", b'{', b'}') else {
        return (Vec::new(), Vec::new());
    };
    let lone = bracket_body(body, "loneCodePoints:", b'[', b']')
        .map(parse_hex_list)
        .unwrap_or_default();
    let ranges = bracket_body(body, "ranges:", b'[', b']')
        .map(parse_range_pairs)
        .unwrap_or_default();
    (lone, ranges)
}

/// Parse the `0x…` literals inside a `loneCodePoints: [ … ]` array.
fn parse_hex_list(content: &str) -> Vec<u32> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while next_hex_prefix(bytes, &mut i) {
        out.push(parse_hex(bytes, &mut i));
    }
    out
}

/// Parse the `[a, b]` pairs inside a `ranges: [[a, b], …]` array.
fn parse_range_pairs(content: &str) -> Vec<(u32, u32)> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            i += 1;
            if !next_hex_prefix(bytes, &mut i) {
                break;
            }
            let a = parse_hex(bytes, &mut i);
            if !next_hex_prefix(bytes, &mut i) {
                break;
            }
            let b = parse_hex(bytes, &mut i);
            out.push((a, b));
        } else {
            i += 1;
        }
    }
    out
}

/// Advance `i` to just past the next `0x` prefix; false at end of input.
fn next_hex_prefix(bytes: &[u8], i: &mut usize) -> bool {
    while *i + 1 < bytes.len() {
        if bytes[*i] == b'0' && (bytes[*i + 1] == b'x' || bytes[*i + 1] == b'X') {
            *i += 2;
            return true;
        }
        *i += 1;
    }
    false
}

/// Parse hex digits at `*i`, advancing past them.
fn parse_hex(bytes: &[u8], i: &mut usize) -> u32 {
    let mut v: u32 = 0;
    while *i < bytes.len() && bytes[*i].is_ascii_hexdigit() {
        v = v * 16 + (bytes[*i] as char).to_digit(16).expect("hex digit");
        *i += 1;
    }
    v
}

/// Decode the `"…"` literals of a `matchStrings: [ … ]` array, unescaping the
/// `\uXXXX` / `\u{…}` / `\xNN` forms the fixtures use.
fn parse_match_strings(src: &str) -> Vec<Vec<u32>> {
    let Some(content) = bracket_body(src, "matchStrings:", b'[', b']') else {
        return Vec::new();
    };
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        i += 1;
        let mut seq = Vec::new();
        while i < bytes.len() && bytes[i] != b'"' {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                let esc = bytes[i + 1];
                i += 2;
                match esc {
                    b'u' if i < bytes.len() && bytes[i] == b'{' => {
                        i += 1;
                        let mut v: u32 = 0;
                        while i < bytes.len() && bytes[i] != b'}' {
                            v = v * 16 + (bytes[i] as char).to_digit(16).expect("hex digit");
                            i += 1;
                        }
                        i += 1; // past '}'
                        seq.push(v);
                    }
                    b'u' => {
                        let mut v: u32 = 0;
                        for _ in 0..4 {
                            v = v * 16 + (bytes[i] as char).to_digit(16).expect("hex digit");
                            i += 1;
                        }
                        seq.push(v);
                    }
                    b'x' => {
                        let mut v: u32 = 0;
                        for _ in 0..2 {
                            v = v * 16 + (bytes[i] as char).to_digit(16).expect("hex digit");
                            i += 1;
                        }
                        seq.push(v);
                    }
                    b'n' => seq.push(0x0A),
                    b't' => seq.push(0x09),
                    b'r' => seq.push(0x0D),
                    b'\\' => seq.push(0x5C),
                    b'"' => seq.push(0x22),
                    other => seq.push(other as u32),
                }
            } else {
                seq.push(bytes[i] as u32);
                i += 1;
            }
        }
        i += 1; // past closing '"'
        out.push(seq);
    }
    out
}

fn render(
    props: &BTreeMap<String, Vec<(u32, u32)>>,
    strings: &BTreeMap<String, Vec<Vec<u32>>>,
) -> String {
    let mut lines: Vec<String> = vec![
        "// Generated by crates/unicode/build.rs from the pinned".into(),
        "// test262 fixtures (Unicode v17). Do not edit by hand.".into(),
        "//".into(),
        "// The code point set of each `\\p{...}` escape the fixtures test,".into(),
        "// as encoded in `property-escapes/generated/*.js`.".into(),
        String::new(),
    ];
    for (name, ranges) in props {
        let ident = name.to_uppercase().replace('-', "_");
        lines.push(format!("/// `\\p{{{name}}}` (from the test262 fixtures)."));
        lines.push(format!("pub const {ident}: &[(u32, u32)] = &["));
        for (a, b) in ranges {
            lines.push(format!("    (0x{a:04X}, 0x{b:04X}),"));
        }
        lines.push("];".into());
        lines.push(String::new());
    }
    lines.push("/// Map a binary-property name to its derived range table.".into());
    lines
        .push("pub fn binary_property_table(name: &str) -> Option<&'static [(u32, u32)]> {".into());
    lines.push("    Some(match name {".into());
    for name in props.keys() {
        let ident = name.to_uppercase().replace('-', "_");
        lines.push(format!("        \"{name}\" => {ident},"));
    }
    lines.push("        _ => return None,".into());
    lines.push("    })".into());
    lines.push("}".into());
    lines.push(String::new());
    lines.push("/// Map a property-of-strings name to its string set.".into());
    for (name, seqs) in strings {
        let ident = name.to_uppercase().replace('-', "_");
        lines.push(format!(
            "/// `\\p{{{name}}}` string set (from the test262 fixtures)."
        ));
        lines.push(format!("pub const {ident}: &[&[u32]] = &["));
        for seq in seqs {
            let cps: Vec<String> = seq.iter().map(|c| format!("0x{c:04X}")).collect();
            lines.push(format!("    &[{}],", cps.join(", ")));
        }
        lines.push("];".into());
        lines.push(String::new());
    }
    lines.push("/// Map a property-of-strings name to its string set.".into());
    lines.push(
        "pub fn property_of_strings(name: &str) -> Option<&'static [&'static [u32]]> {".into(),
    );
    lines.push("    Some(match name {".into());
    for name in strings.keys() {
        let ident = name.to_uppercase().replace('-', "_");
        lines.push(format!("        \"{name}\" => {ident},"));
    }
    lines.push("        _ => return None,".into());
    lines.push("    })".into());
    lines.push("}".into());
    lines.push(String::new());
    let mut out = lines.join("\n");
    out.push('\n');
    out
}
