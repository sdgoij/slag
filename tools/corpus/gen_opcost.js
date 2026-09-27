// Generates the `opcost/` family: one self-contained workload per operation.
//
// Every row runs the same iteration count, so a row's per-call time is
// directly comparable with any other row's, and the `baseline` row (the bare
// loop) can be subtracted to read what one operation costs *on its own* — on
// each engine, and in each mode. That is the point of the family: it
// separates "the builtin's body is slow" from "the call and access machinery
// around it is slow", which the family-level gap table cannot.
//
// Each row declares ONLY the setup its body needs. A shared prologue is why
// this family could not measure the JIT it was written to measure: a body
// above the runtime's compile cap (JIT_MAX_COMPILE_STEPS) is never compiled,
// so its `jit` column was an interpreted time, and the shared prologue was
// most of the step count. The rule is therefore per-row setup, small enough
// that every row compiles in BOTH profiles — verified with `JIT_DUMP_CLIF=1`
// on a debug binary, which prints `jit skip: body too large (N steps)` for
// any row over the cap. A row that needs a 1024-entry collection pays for
// exactly that one fill loop and nothing else.
//
// A separate consequence of per-row setup: rows no longer share a frame
// shape, so the `baseline` subtraction assumes the bare loop's cost is
// insensitive to how many locals the frame holds. Measured rather than
// asserted: the same bare loop with 2 locals, with a Map fill, and with 15
// further locals plus objects, an array, a string, a regexp and a function,
// differ by less than the round-to-round noise (the bare loop itself moves
// ±10% between rounds). Every row also clears the cap by a wide margin — the
// largest is under 64 steps, which the debug binary confirms by compiling all
// 33 of them with its cap temporarily cut to 64.
//
// The bodies end in `| 0`, which keeps the accumulator an int32. That does two
// things: the four modes agree on the result (so the parity check is a real
// check), and V8 cannot closed-form-fold the arithmetic the way it does for
// the affine loops the README warns about.
//
// Usage: node gen_opcost.js   (writes ./workloads/opcost/)
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const N = 100000;
const out = path.join(__dirname, 'workloads', 'opcost');

const STRS = 'var strs = ["abcdefgh", "ijklmnop", "qrstuvwx", "yz012345"];';
const arr = [
  'var arr = new Array(1024);',
  // Not arr[i] = i on purpose: element_read's value would then equal
  // baseline's, and a mis-wired body would slip past the parity check.
  'for (var i = 0; i < 1024; i++) arr[i] = 1024 - i;',
].join('\n  ');
const mapFor = (name) =>
  [`var ${name} = new Map();`, `for (var i = 0; i < 1024; i++) ${name}.set(i, i);`].join('\n  ');
const set = ['var set = new Set();', 'for (var i = 0; i < 1024; i++) set.add(i);'].join('\n  ');

// [name, setup lines, loop body]. `k` is `i & 1023`, in range for the
// 1024-element array, the 1024-entry Map and Set, and the 8-character strings.
//
// Two rows are localizers for the builtin gap, and they exist to be
// subtracted rather than read alone: `method_call` (an own data property
// holding a JS function) against `proto_method_call` (the same function on a
// prototype) isolates what resolving a method through the chain costs, and
// `map_get` (a builtin reached through `%Map.prototype%`) against
// `own_builtin_call` (the same builtin, same receiver, same body, found as an
// own data property) isolates it for a builtin. `js_call` and `math_abs` bound
// the builtin-call premium from the other side.
const rows = [
  ['baseline', '', 's = (s + k) | 0;'],
  // Creation: the family measured element/property access but never the
  // object the access is on, and allocation is what dominates every builtin
  // whose body builds a container (`Object.keys`, `JSON.stringify`, a
  // spread). `arr.length = 0` is the other half — the length-write path.
  ['array_alloc', '', 's = (s + [k].length) | 0;'],
  ['object_alloc', '', 's = (s + ({ a: 1 }).a) | 0;'],
  ['array_length_write', 'var arr = [1, 2, 3];', 'arr.length = 0; s = (s + arr.length) | 0;'],
  // The two paths that reach the interner without a cached atom: a computed
  // read whose key is a *string value* (ToString -> intern, once per read),
  // and any array builtin (they all read `length` through a per-call
  // `JsString::from_utf8`, so `indexOf` is the cheapest representative).
  [
    'dyn_key_read',
    STRS +
      '\n  var props = { abcdefgh: 1, ijklmnop: 2, qrstuvwx: 3, yz012345: 4 };',
    's = (s + props[strs[k & 3]]) | 0;',
  ],
  ['array_indexof', 'var small = [1, 2, 3, 4];', 's = (s + small.indexOf(3)) | 0;'],
  ['array_includes', 'var small = [1, 2, 3, 4];', 's = (s + (small.includes(4) ? 1 : 0)) | 0;'],
  [
    'array_for_each',
    'var small = [1, 2, 3, 4];\n  var cb = function (x) { return x + 1; };',
    'small.forEach(cb); s = (s + 1) | 0;',
  ],
  ['array_at', 'var small = [1, 2, 3, 4];', 's = (s + small.at(2)) | 0;'],
  // The write side: `fill` is pure writes, `reverse` a read/write pair, so a
  // change to one half shows up in one row and not the other.
  ['array_fill', 'var small = [1, 2, 3, 4];', 'small.fill(0); s = (s + 1) | 0;'],
  ['array_reverse', 'var small = [1, 2, 3, 4];', 'small.reverse(); s = (s + small.length) | 0;'],
  ['array_slice', 'var small = [1, 2, 3, 4];', 's = (s + small.slice(1, 3).length) | 0;'],
  [
    'array_to_sorted',
    'var small = [4, 2, 3, 1];',
    's = (s + small.toSorted().length) | 0;',
  ],
  ['element_read', arr, 's = (s + arr[k]) | 0;'],
  ['element_write', arr, 'arr[k] = i; s = (s + 1) | 0;'],
  ['obj_prop', 'var o = { x: 7 };', 's = (s + o.x) | 0;'],
  ['prim_prop', STRS, 's = (s + strs[k & 3].length) | 0;'],
  ['js_call', 'var f = function (x) { return x + 1; };', 's = (s + f(k)) | 0;'],
  [
    'method_call',
    'var obj = { m: function (x) { return x + 1; } };',
    's = (s + obj.m(k)) | 0;',
  ],
  [
    'proto_method_call',
    'function P() {}\n  P.prototype.get = function (x) { return x + 1; };\n  var p = new P();',
    's = (s + p.get(k)) | 0;',
  ],
  ['math_abs', '', 's = (s + Math.abs(k)) | 0;'],
  ['num_valueof', '', 's = (s + (k).valueOf()) | 0;'],
  ['map_get', mapFor('m'), 's = (s + m.get(k)) | 0;'],
  [
    'own_builtin_call',
    mapFor('mg') + '\n  mg.get = Map.prototype.get;',
    's = (s + mg.get(k)) | 0;',
  ],
  ['map_set', mapFor('m'), 'm.set(k, i); s = (s + 1) | 0;'],
  ['set_has', set, 's = (s + (set.has(k) ? 1 : 0)) | 0;'],
  ['regexp_test', STRS + '\n  var re = /[a-z]+/;', 's = (s + (re.test(strs[k & 3]) ? 1 : 0)) | 0;'],
  ['string_charat', STRS, 's = (s + strs[k & 3].charCodeAt(k & 7)) | 0;'],
  ['string_indexof', STRS, 's = (s + strs[k & 3].indexOf("cd")) | 0;'],
  [
    'array_push',
    'var pushArr = [];',
    'pushArr.push(k); if (pushArr.length > 4096) pushArr.length = 0; s = (s + 1) | 0;',
  ],
  ['math_call', '', 's = (s + Math.floor(Math.sqrt(k))) | 0;'],
  ['json_stringify', 'var o2 = { a: 1, b: 2, c: 3 };', 's = (s + JSON.stringify(o2).length) | 0;'],
  ['object_keys', 'var o3 = { a: 1, b: 2, c: 3, d: 4 };', 's = (s + Object.keys(o3).length) | 0;'],
];

const template = (setup, body) => `// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  ${setup}
  var s = 0;
  for (var i = 0; i < ${N}; i++) {
    var k = i & MASK;
    ${body}
  }
  return s;
}
bench();
`;

fs.mkdirSync(out, { recursive: true });
for (const [name, setup, body] of rows) {
  fs.writeFileSync(path.join(out, name + '.js'), template(setup, body));
}
console.log('wrote ' + rows.length + ' rows to ' + out);
