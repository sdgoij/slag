// Generates the `opcost/` family: one self-contained workload per operation.
//
// Every row runs the same iteration count, so a row's per-call time is
// directly comparable with any other row's, and the `baseline` row (the bare
// loop) can be subtracted to read what one operation costs *on its own* — on
// each engine, and in each mode. That is the point of the family: it
// separates "the builtin's body is slow" from "the call and access machinery
// around it is slow", which the family-level gap table cannot.
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

// [name, loop body]. `k` is `i & 1023`, in range for the 1024-element array, the
// 1024-entry Map and Set, and the 8-character strings.
const rows = [
  ['baseline', 's = (s + k) | 0;'],
  ['element_read', 's = (s + arr[k]) | 0;'],
  ['element_write', 'arr[k] = i; s = (s + 1) | 0;'],
  ['obj_prop', 's = (s + o.x) | 0;'],
  ['prim_prop', 's = (s + strs[k & 3].length) | 0;'],
  ['js_call', 's = (s + f(k)) | 0;'],
  ['method_call', 's = (s + obj.m(k)) | 0;'],
  ['math_abs', 's = (s + Math.abs(k)) | 0;'],
  ['num_valueof', 's = (s + (k).valueOf()) | 0;'],
  ['map_get', 's = (s + m.get(k)) | 0;'],
  ['map_set', 'm.set(k, i); s = (s + 1) | 0;'],
  ['set_has', 's = (s + (set.has(k) ? 1 : 0)) | 0;'],
  ['regexp_test', 's = (s + (re.test(strs[k & 3]) ? 1 : 0)) | 0;'],
  ['string_charat', 's = (s + strs[k & 3].charCodeAt(k & 7)) | 0;'],
  ['string_indexof', 's = (s + strs[k & 3].indexOf("cd")) | 0;'],
  [
    'array_push',
    'pushArr.push(k); if (pushArr.length > 4096) pushArr.length = 0; s = (s + 1) | 0;',
  ],
  ['math_call', 's = (s + Math.floor(Math.sqrt(k))) | 0;'],
  ['json_stringify', 's = (s + JSON.stringify(o2).length) | 0;'],
  ['object_keys', 's = (s + Object.keys(o3).length) | 0;'],
];

const template = `// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var arr = new Array(1024);
  // Not arr[i] = i on purpose: element_read's value would then equal
  // baseline's, and a mis-wired body would slip past the parity check.
  for (var i = 0; i < 1024; i++) arr[i] = 1024 - i;
  var m = new Map();
  for (var i = 0; i < 1024; i++) m.set(i, i);
  var set = new Set();
  for (var i = 0; i < 1024; i++) set.add(i);
  var o = { x: 7 };
  var obj = { m: function (x) { return x + 1; } };
  var f = function (x) { return x + 1; };
  var strs = ["abcdefgh", "ijklmnop", "qrstuvwx", "yz012345"];
  var re = /[a-z]+/;
  var o2 = { a: 1, b: 2, c: 3 };
  var o3 = { a: 1, b: 2, c: 3, d: 4 };
  var pushArr = [];
  var s = 0;
  for (var i = 0; i < ${N}; i++) {
    var k = i & MASK;
    BODY
  }
  return s;
}
bench();
`;

fs.mkdirSync(out, { recursive: true });
for (const [name, body] of rows) {
  fs.writeFileSync(path.join(out, name + '.js'), template.replace('BODY', body));
}
console.log('wrote ' + rows.length + ' rows to ' + out);
