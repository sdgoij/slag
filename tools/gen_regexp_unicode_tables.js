// Generate crates/unicode/src/derived_regexp_tables.rs from the pinned
// test262 fixtures. A port of tools/gen_regexp_unicode_tables.py,
// dogfooded: it runs on Slag itself.
//
// Usage (from the repository root):
//   slag tools/gen_regexp_unicode_tables.js
// Writes: crates/unicode/src/derived_regexp_tables.rs
//
// Each `property-escapes/generated/<Name>.js` fixture encodes the exact code
// point set of the Unicode binary property / general-category value / script
// it tests: `buildString({ loneCodePoints: [...], ranges: [[a, b], ...] })`.
// The property-of-strings fixtures (`property-escapes/generated/strings/*.js`
// and the `unicodeSets/generated/rgi-emoji-*.js` files) encode their string
// sets as `matchStrings: ["\\uXXXX...", ...]`. test262 is pinned to a
// Unicode version (fixtures are generated from Unicode v17), so these tables
// are the exact data the corpus asserts; the engine's hand-written predicates
// (via unicode-properties / unicode-script / unicode-id crates) remain for
// the properties they cover.

const GEN = 'test262/test/built-ins/RegExp/property-escapes/generated';
const SETGEN = 'test262/test/built-ins/RegExp/unicodeSets/generated';
const OUT = 'crates/unicode/src/derived_regexp_tables.rs';

// Hex the way Rust's {:04X} does: uppercase, zero-padded to at least 4
// digits (larger values keep every digit).
function hex4(n) {
  let h = n.toString(16).toUpperCase();
  while (h.length < 4) {
    h = '0' + h;
  }
  return h;
}

// Decode a JS string literal (the fixtures use \uXXXX and \u{XXXXX}).
function decodeJsString(lit) {
  const out = [];
  let i = 0;
  while (i < lit.length) {
    const ch = lit[i];
    if (ch === '\\' && i + 1 < lit.length) {
      const nxt = lit[i + 1];
      if (nxt === 'u') {
        if (i + 2 < lit.length && lit[i + 2] === '{') {
          const end = lit.indexOf('}', i + 3);
          out.push(parseInt(lit.slice(i + 3, end), 16));
          i = end + 1;
        } else {
          out.push(parseInt(lit.slice(i + 2, i + 6), 16));
          i += 6;
        }
        continue;
      }
      if (nxt === 'x') {
        out.push(parseInt(lit.slice(i + 2, i + 4), 16));
        i += 4;
        continue;
      }
      if (nxt === 'n') { out.push(0x0a); i += 2; continue; }
      if (nxt === 't') { out.push(0x09); i += 2; continue; }
      if (nxt === 'r') { out.push(0x0d); i += 2; continue; }
      if (nxt === '\\') { out.push(0x5c); i += 2; continue; }
      if (nxt === '"') { out.push(0x22); i += 2; continue; }
      out.push(nxt.codePointAt(0));
      i += 2;
      continue;
    }
    out.push(ch.codePointAt(0));
    i += 1;
  }
  return out;
}

// Extract loneCodePoints + ranges from a buildString({...}) call.
function parseBuildString(src) {
  const m = src.match(/buildString\(\{([^}]*)\}\)/s);
  const body = m ? m[1] : '';
  const lone = [];
  const ranges = [];
  const lm = body.match(/loneCodePoints:\s*\[([^\]]*)\]/);
  if (lm) {
    const hexes = lm[1].match(/0x([0-9A-Fa-f]+)/g);
    if (hexes) {
      for (const h of hexes) {
        lone.push(parseInt(h.slice(2), 16));
      }
    }
  }
  const rm = body.match(/ranges:\s*/);
  if (rm) {
    // Depth scan from the opening `[` to its matching `]` (two levels of
    // nesting: [[a, b], [c, d], ...]).
    const start = rm.index + rm[0].length;
    let depth = 0;
    let i = start;
    while (i < body.length && depth >= 0) {
      if (body[i] === '[') depth += 1;
      else if (body[i] === ']') depth -= 1;
      if (depth === 0 && i > start) break;
      i += 1;
    }
    const seg = body.slice(start, i + 1);
    const pairs = seg.match(/\[0x([0-9A-Fa-f]+),\s*0x([0-9A-Fa-f]+)\]/g);
    if (pairs) {
      for (const p of pairs) {
        const mm = p.match(/0x([0-9A-Fa-f]+),\s*0x([0-9A-Fa-f]+)/);
        ranges.push([parseInt(mm[1], 16), parseInt(mm[2], 16)]);
      }
    }
  }
  return [lone, ranges];
}

// Extract the matchStrings array of a testPropertyOfStrings call.
function parseMatchStrings(src) {
  const m = src.match(/matchStrings:\s*\[(.*?)\n\s*\]/s);
  if (!m) {
    return [];
  }
  const out = [];
  const lits = m[1].match(/"[^"]*"/g);
  if (lits) {
    for (const lit of lits) {
      out.push(decodeJsString(lit.slice(1, -1)));
    }
  }
  return out;
}

const props = {};   // canonical name -> sorted code point array
const strings = {}; // canonical name -> list of code point arrays

for (const fn of fs.readdirSync(GEN).sort()) {
  if (!fn.endsWith('.js')) continue;
  const name = fn.slice(0, -3);
  const src = fs.readFileSync(GEN + '/' + fn);
  const [lone, ranges] = parseBuildString(src);
  if (lone.length || ranges.length) {
    const pts = new Set(lone);
    for (const [a, b] of ranges) {
      for (let cp = a; cp <= b; cp++) pts.add(cp);
    }
    props[name] = Array.from(pts).sort((x, y) => x - y);
  }
}

// Property-of-strings fixtures live in the `strings/` subdirectory.
for (const fn of fs.readdirSync(GEN + '/strings').sort()) {
  if (!fn.endsWith('.js')) continue;
  const name = fn.slice(0, -3);
  const ms = parseMatchStrings(fs.readFileSync(GEN + '/strings/' + fn));
  if (ms.length) strings[name] = ms;
}

// The `unicodeSets/generated/rgi-emoji-*.js` fixtures are version-specific
// subsets of RGI_Emoji; `strings/RGI_Emoji.js` is the authoritative full
// set (3953 sequences vs the ~650-version subsets), so it wins.
for (const fn of fs.readdirSync(SETGEN).sort()) {
  if (!fn.startsWith('rgi-emoji-')) continue;
  const ms = parseMatchStrings(fs.readFileSync(SETGEN + '/' + fn));
  if (ms.length && !('RGI_Emoji' in strings)) {
    strings.RGI_Emoji = (strings.RGI_Emoji || []).concat(ms);
  }
}

const lines = [];
lines.push('//! Generated by tools/gen_regexp_unicode_tables.js from the');
lines.push('//! pinned test262 fixtures (Unicode v17). Do not edit by hand.');
lines.push('//!');
lines.push('//! The code point set of each `\\p{...}` escape the fixtures test,');
lines.push('//! as encoded in `property-escapes/generated/*.js`.');
lines.push('#![allow(clippy::unreadable_literal)]');
lines.push('');

for (const name of Object.keys(props).sort()) {
  const pts = props[name];
  const ranges = [];
  for (const cp of pts) {
    const last = ranges[ranges.length - 1];
    if (ranges.length && cp === last[1] + 1) {
      last[1] = cp;
    } else {
      ranges.push([cp, cp]);
    }
  }
  const ident = name.toUpperCase().replace(/-/g, '_');
  lines.push('/// `\\p{' + name + '}` (from the test262 fixtures).');
  lines.push('pub const ' + ident + ': &[(u32, u32)] = &[');
  for (const [a, b] of ranges) {
    lines.push('    (0x' + hex4(a) + ', 0x' + hex4(b) + '),');
  }
  lines.push('];');
  lines.push('');
}

lines.push('/// Map a binary-property name to its derived range table.');
lines.push("pub fn binary_property_table(name: &str) -> Option<&'static [(u32, u32)]> {");
lines.push('    Some(match name {');
for (const name of Object.keys(props).sort()) {
  lines.push('        "' + name + '" => ' + name.toUpperCase().replace(/-/g, '_') + ',');
}
lines.push('        _ => return None,');
lines.push('    })');
lines.push('}');
lines.push('');

lines.push('/// Map a property-of-strings name to its string set.');
for (const name of Object.keys(strings).sort()) {
  const ident = name.toUpperCase().replace(/-/g, '_');
  lines.push('/// `\\p{' + name + '}` string set (from the test262 fixtures).');
  lines.push('pub const ' + ident + ': &[&[u32]] = &[');
  for (const s of strings[name]) {
    const cps = [];
    for (const c of s) {
      cps.push('0x' + hex4(c));
    }
    lines.push('    &[' + cps.join(', ') + '],');
  }
  lines.push('];');
  lines.push('');
}
lines.push('/// Map a property-of-strings name to its string set.');
lines.push("pub fn property_of_strings(name: &str) -> Option<&'static [&'static [u32]]> {");
lines.push('    Some(match name {');
for (const name of Object.keys(strings).sort()) {
  lines.push('        "' + name + '" => ' + name.toUpperCase().replace(/-/g, '_') + ',');
}
lines.push('        _ => return None,');
lines.push('    })');
lines.push('}');
lines.push('');

fs.writeFileSync(OUT, lines.join('\n') + '\n');
'wrote ' + OUT + ': ' + Object.keys(props).length + ' binary props, ' +
  Object.keys(strings).length + ' string sets';
