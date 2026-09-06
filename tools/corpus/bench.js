// Corpus orchestrator: run every workload under four engine modes (slag jit,
// slag jitless, node jit, node jitless), verify all four produce the same
// result, and print a per-workload + per-family gap table.
//
// Usage:  node bench.js [--slag <path-to-slag-binary>]
//
// The slag binary defaults to target/release/slag.exe (or $SLAG_BIN). The
// node side runs through run_node.js in this directory. Workloads live in
// ./workloads/<family>/<name>.js; each must define `function bench() {...}`
// ending with `bench();` and return a Number.
'use strict';

const { spawnSync } = require('node:child_process');
const path = require('node:path');

const dir = path.join(__dirname, 'workloads');
const runner = path.join(__dirname, 'run_node.js');
const node = process.execPath;

let slag = process.env.SLAG_BIN || path.join(__dirname, '..', '..', 'target', 'release', 'slag.exe');
const flagAt = process.argv.indexOf('--slag');
if (flagAt >= 0 && process.argv[flagAt + 1]) {
  slag = process.argv[flagAt + 1];
}

const runs = [
  { engine: 'slag', mode: 'jit', cmd: slag, args: ['--corpus', dir] },
  { engine: 'slag', mode: 'jitless', cmd: slag, args: ['--jitless', '--corpus', dir] },
  { engine: 'node', mode: 'jit', cmd: node, args: [runner, dir] },
  { engine: 'node', mode: 'jitless', cmd: node, args: ['--jitless', runner, dir] },
];

// name -> { slag: { jit: cell, jitless: cell }, node: { ... } }
const results = {};

for (const r of runs) {
  const res = spawnSync(r.cmd, r.args, { encoding: 'utf8', maxBuffer: 1 << 26 });
  if (res.error) {
    console.error('spawn ' + r.engine + '.' + r.mode + ': ' + res.error.message);
    process.exit(1);
  }
  if (res.status !== 0) {
    console.error(r.engine + '.' + r.mode + ' exited ' + res.status + '\nstderr:\n' + res.stderr);
    process.exit(1);
  }
  for (const line of res.stdout.split('\n')) {
    const parts = line.split('\t');
    if (parts[0] !== 'bench' || parts.length < 5) {
      continue;
    }
    const [, mode, name, ms, value] = parts;
    if (mode !== r.mode) {
      continue;
    }
    const entry = (results[name] = results[name] || { slag: {}, node: {} });
    entry[r.engine][r.mode] = { ms: Number(ms), value };
  }
}

const names = Object.keys(results).sort();
const fmt = (x) => (x == null || !isFinite(x) ? '      -' : x.toFixed(1).padStart(7));
const fmtGap = (x) => (x == null || !isFinite(x) || x <= 0 ? '    -' : x.toFixed(2).padStart(6));

console.log('slag: ' + slag);
console.log('node: ' + process.version);
console.log('workloads: ' + names.length + ' (' + dir + ')');
console.log('result parity = all four modes return the same value; NA = non-number result');
console.log('');
const header =
  'workload'.padEnd(34) +
  ' slag-jit' + ' slag-jl' + ' node-jit' + ' node-jl' +
  ' jitGap' + ' jlGap' + ' parity';
console.log(header);
console.log('-'.repeat(header.length));

const familyAgg = {};

for (const name of names) {
  const cell = (engine, mode) => results[name][engine] && results[name][engine][mode];
  const sj = cell('slag', 'jit');
  const sjl = cell('slag', 'jitless');
  const nj = cell('node', 'jit');
  const njl = cell('node', 'jitless');

  const all = [sj, sjl, nj, njl];
  const missing = all.some((c) => !c);
  const numeric = all.map((c) => (c && c.value !== 'NA' ? Number(c.value) : NaN));
  const parity = !missing && numeric.every((v) => (isNaN(v) && isNaN(numeric[0])) || v === numeric[0]);
  const jitGap = sj && nj ? sj.ms / nj.ms : NaN;
  const jlGap = sjl && njl ? sjl.ms / njl.ms : NaN;

  const family = name.split('/')[0];
  const agg = (familyAgg[family] = familyAgg[family] || { jit: [], jl: [], n: 0, mismatch: 0 });
  agg.n += 1;
  if (!parity) {
    agg.mismatch += 1;
  }
  if (isFinite(jitGap)) {
    agg.jit.push(jitGap);
  }
  if (isFinite(jlGap)) {
    agg.jl.push(jlGap);
  }

  console.log(
    name.padEnd(34) +
      fmt(sj && sj.ms) +
      fmt(sjl && sjl.ms) +
      fmt(nj && nj.ms) +
      fmt(njl && njl.ms) +
      fmtGap(jitGap) +
      fmtGap(jlGap) +
      (parity ? '  ok' : '  ' + (missing ? 'ERR' : 'MISMATCH'))
  );
}

console.log('');
console.log('family summary (gap = slag / node ms; <1 means slag faster)');
console.log(
  'family'.padEnd(16) +
    ' n' +
    '  mean-jitGap'.padStart(14) +
    '  mean-jlGap'.padStart(14) +
    '  mismatches'.padStart(12)
);
for (const family of Object.keys(familyAgg).sort()) {
  const agg = familyAgg[family];
  const mean = (xs) => (xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : NaN);
  const meanJit = mean(agg.jit);
  const meanJl = mean(agg.jl);
  console.log(
    family.padEnd(16) +
      String(agg.n).padStart(4) +
      (isFinite(meanJit) ? meanJit.toFixed(2).padStart(14) : '             -') +
      (isFinite(meanJl) ? meanJl.toFixed(2).padStart(14) : '             -') +
      String(agg.mismatch).padStart(12)
  );
}

const allJit = [];
const allJl = [];
let mismatches = 0;
for (const name of names) {
  const cell = (engine, mode) => results[name][engine] && results[name][engine][mode];
  const sj = cell('slag', 'jit');
  const sjl = cell('slag', 'jitless');
  const nj = cell('node', 'jit');
  const njl = cell('node', 'jitless');
  if (sj && nj && sj.ms > 0) {
    allJit.push(sj.ms / nj.ms);
  }
  if (sjl && njl && sjl.ms > 0) {
    allJl.push(sjl.ms / njl.ms);
  }
  const all = [sj, sjl, nj, njl];
  const numeric = all.map((c) => (c && c.value !== 'NA' ? Number(c.value) : NaN));
  if (!all.every(Boolean) || !numeric.every((v) => (isNaN(v) && isNaN(numeric[0])) || v === numeric[0])) {
    mismatches += 1;
  }
}
const mean = (xs) => (xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : NaN);
console.log('');
console.log(
  'overall: n=' + names.length +
    '  mean-jitGap=' + (isFinite(mean(allJit)) ? mean(allJit).toFixed(2) : '-') +
    '  mean-jlGap=' + (isFinite(mean(allJl)) ? mean(allJl).toFixed(2) : '-') +
    '  mismatches=' + mismatches
);
