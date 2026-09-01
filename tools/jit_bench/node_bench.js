// Node.js harness for the slag --jit-bench suite.
// Usage: node node_bench.js            (JIT mode)
//        node --jitless node_bench.js  (interpreter-only mode)
//
// Methodology: each snippet is `function bench(...) {...} bench(args);`.
// We eval the function definition once, bind its arguments once, warm the
// function up with a few calls (so V8 tiers up to optimized code when the
// JIT is enabled), then time repeated calls. 3 rounds of 5 timed calls,
// per-call mean reported. Mirrors slag's bench_once (1 warmup + 1 timed
// eval), with extra warmup so the Node JIT column measures steady state.

'use strict';

const { performance } = require('node:perf_hooks');

const ROUNDS = 3;
const WARMUP = 3;
const TIMED = 5;

const benchmarks = [
  [
    'arithmetic',
    'function bench() { var n = 0; for (var i = 0; i < 1000000; i++) { n += i * 2; } return n; } bench();',
  ],
  [
    'property read',
    'function bench(o) { var n = 0; for (var i = 0; i < 1000000; i++) { n += o.a + o.b; } return n; } bench({ a: 1, b: 2 });',
  ],
  [
    'string concat',
    "function bench(x) { var s = x; for (var i = 0; i < 100000; i++) { s += x; } return s.length; } bench('x');",
  ],
  [
    'function calls',
    'function bench(o, n) { var s = 0; for (var i = 0; i < n; i++) { s += o.f(i); } return s; }\n\
bench({ f: function (x) { return x + 1; } }, 100000);',
  ],
  [
    'global read',
    'var g = 1; function bench(n) { var s = 0; for (var i = 0; i < n; i++) { s += g; } return s; } bench(1000000);',
  ],
  [
    'compound assign',
    'function bench(o, n) { var s = 0; for (var i = 0; i < n; i++) { o.x += 1; s += o.x; } return s; }\n\
bench({ x: 0 }, 100000);',
  ],
  [
    'buildString shape',
    'function bench() { var a = []; var l = 0; var c = 0; for (var i = 0; i < 3000000; i++) { a[l++] = i; if (l === 10000) { c++; a.length = l = 0; } } return c; } bench();',
  ],
  [
    'buildString full',
    'function bench() { var lone = [0x2D, 0x58A, 0x5BE, 0x1400, 0x1806, 0x2053, 0x207B, 0x208B, 0x2212, 0x2E17, 0x2E1A, 0x2E40, 0x2E5D, 0x301C, 0x3030, 0x30A0, 0xFE58, 0xFE63, 0xFF0D, 0x10D6E, 0x10EAD]; var ranges = [[0xDC00, 0xDFFF], [0x0, 0x2C], [0x2E, 0x589], [0x58B, 0x5BD], [0x5BF, 0x13FF], [0x1401, 0x1805], [0x1807, 0x200F], [0x2016, 0x2052], [0x2054, 0x207A], [0x207C, 0x208A], [0x208C, 0x2211], [0x2213, 0x2E16], [0x2E18, 0x2E19], [0x2E1B, 0x2E39], [0x2E3C, 0x2E3F], [0x2E41, 0x2E5C], [0x2E5E, 0x301B], [0x301D, 0x302F], [0x3031, 0x309F], [0x30A1, 0xDBFF], [0xE000, 0xFE30], [0xFE33, 0xFE57], [0xFE59, 0xFE62], [0xFE64, 0xFF0C], [0xFF0E, 0x10D6D], [0x10D6F, 0x10EAC], [0x10EAE, 0x10FFFF]]; var CHUNK = 10000; var result = String.fromCodePoint.apply(null, lone); for (var i = 0; i < ranges.length; i++) { var start = ranges[i][0]; var end = ranges[i][1]; var codePoints = []; for (var length = 0, codePoint = start; codePoint <= end; codePoint++) { codePoints[length++] = codePoint; if (length === CHUNK) { result += String.fromCodePoint.apply(null, codePoints); codePoints.length = length = 0; } } result += String.fromCodePoint.apply(null, codePoints); } return result.length; } bench();',
  ],
  [
    'typed-array write',
    'function bench(ta) { for (var k = 0; k < ta.length; k++) { ta[k] = k & 255; } return ta.length; } bench(new Uint8Array(800000));',
  ],
  [
    'typed-array length',
    'function bench(ta) { var s = 0; for (var k = 0; k < ta.length; k++) { s += ta.length; } return s; } bench(new Uint8Array(800000));',
  ],
  [
    'vector leaf call',
    'function bench(f) { var s = 0; for (var i = 0; i < 200000; i++) { s += f(i, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33); } return s; } bench(function (a, b, c, d, e, g, h, k, l, m, n, o, p, q, r, t, u, v, w, x, y, z, A, B, C, D, E, F, G, H, I, J, K) { return a + 1; });',
  ],
  [
    'apply leaf call',
    'function bench(f) { var s = 0; var arr = [1, 2, 3, 4, 5, 6, 7, 8, 9]; for (var i = 0; i < 200000; i++) { s += f.apply(null, arr); } return s; } bench(function (a, b, c, d, e, g, h, k, l) { return a + 1; });',
  ],
];

// Split `function bench(...) {...} bench(EXPR);` at the LAST `bench(` so we
// can bind the arguments once and call the function object repeatedly.
function setup(src) {
  const idx = src.lastIndexOf('bench(');
  const defPart = src.slice(0, idx);
  const callPart = src.slice(idx); // "bench(EXPR);"
  eval(defPart + ';globalThis.__bench = bench;');
  const args = eval('[' + callPart.slice('bench('.length, -2) + ']');
  const fn = globalThis.__bench;
  return { fn, args };
}

function timeCall(fn, args) {
  const t0 = performance.now();
  const result = fn(...args);
  const t1 = performance.now();
  return { ms: t1 - t0, result };
}

console.log('node ' + process.version + ' mode=' + (process.execArgv.includes('--jitless') ? 'jitless' : 'jit'));
for (const [name, src] of benchmarks) {
  const { fn, args } = setup(src);
  for (let r = 0; r < ROUNDS; r++) {
    let last;
    for (let w = 0; w < WARMUP; w++) last = fn(...args);
    let total = 0;
    for (let t = 0; t < TIMED; t++) {
      const { ms, result } = timeCall(fn, args);
      total += ms;
      last = result;
    }
    const perCall = total / TIMED;
    console.log(
      name.padEnd(18) + ' round ' + (r + 1) + '  ' + perCall.toFixed(4).padStart(10) + ' ms/call  result ' + last
    );
  }
}
