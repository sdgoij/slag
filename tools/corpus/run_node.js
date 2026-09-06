// Node runner for the workload corpus — the mirror of slag's --corpus mode.
//
// Usage:  node run_node.js <workloads-dir>           (JIT)
//         node --jitless run_node.js <workloads-dir>  (interpreter)
//
// Protocol mirrors the CLI bench_once exactly so the two engines measure the
// same thing: each workload file defines `function bench(...) {...}` and ends
// with `bench(ARGS);`. The definition is evaluated once, the arguments are
// bound once, the function is warmed with 2 calls (paying any compile /
// tier-up), then 3 timed calls report the MIN per-call time (the min, not the
// mean — the mean is skewed by the GC pressure the previous timed calls'
// garbage creates). Prints one line per workload:
//
//   bench<TAB><mode><TAB><relpath><TAB><ms><TAB><result>
//
// <mode> is jit or jitless (from the process exec args); <result> is the
// bench() completion value when it is a Number, else NA.
// NOTE: this file must stay sloppy — a direct eval is used to load each
// workload's `function bench` into this module's scope (a `'use strict'`
// directive would scope the eval's declarations to the eval itself).

const fs = require('node:fs');
const path = require('node:path');
const { performance } = require('node:perf_hooks');

const WARMUP = 2;
const TIMED = 3;
const MODE = process.execArgv.includes('--jitless') ? 'jitless' : 'jit';

const dir = process.argv[2];
if (!dir) {
  console.error('run_node.js: missing workloads directory');
  process.exit(1);
}

function collect(root, out) {
  for (const entry of fs.readdirSync(root, { withFileTypes: true }).sort((a, b) =>
    a.name < b.name ? -1 : a.name > b.name ? 1 : 0
  )) {
    const p = path.join(root, entry.name);
    if (entry.isDirectory()) {
      collect(p, out);
    } else if (entry.name.endsWith('.js')) {
      out.push(p);
    }
  }
}

// Split `function bench(...) {...} bench(ARGS);` at the LAST `bench(` so the
// definition is evaluated once and the args are bound once (same as bench_once).
function benchOnce(source) {
  const WARMUP = 2;
  const TIMED = 3;
  const MAX_REPS = 1 << 20;
  const FLOOR_MS = 20; // each timed sample should clear this on every engine
  // The arg split assumes the source ends `);` — trim trailing whitespace so
  // files ending with a newline after `bench();` split identically to rows.
  const trimmed = source.trimEnd();
  const callAt = trimmed.lastIndexOf('bench(');
  if (callAt < 0) {
    throw new Error('no bench(...) invocation');
  }
  const def = trimmed.slice(0, callAt);
  const argsSrc = trimmed.slice(callAt + 'bench('.length, trimmed.length - 2);
  eval(def); // eslint-disable-line no-eval
  globalThis.__bench = bench;
  const args = eval('[' + argsSrc + ']'); // eslint-disable-line no-eval
  const fn = globalThis.__bench;
  for (let w = 0; w < WARMUP; w++) {
    fn(...args);
  }
  // Batch sizing mirrors the CLI bench_once: a single call may be far below
  // the floor (V8 closed-form-folds the affine loops, so a per-call time is
  // independent of the iteration count), so time batches of `reps` calls and
  // report the per-call min.
  let reps = 1;
  {
    const t0 = performance.now();
    fn(...args);
    const single = performance.now() - t0;
    if (single > 0 && single < FLOOR_MS) {
      reps = Math.min(MAX_REPS, Math.max(1, Math.ceil(FLOOR_MS / single)));
    }
  }
  if (reps > 1) {
    for (let w = 0; w < reps; w++) {
      fn(...args);
    }
  }
  let best = Infinity;
  let value;
  for (let t = 0; t < TIMED; t++) {
    const t0 = performance.now();
    for (let r = 0; r < reps; r++) {
      value = fn(...args);
    }
    const perCall = (performance.now() - t0) / reps;
    if (perCall < best) {
      best = perCall;
    }
  }
  return { ms: best, value };
}

const files = [];
collect(dir, files);
for (const file of files) {
  const source = fs.readFileSync(file, 'utf8');
  if (!source.trim()) {
    continue;
  }
  let r;
  try {
    r = benchOnce(source);
  } catch (e) {
    console.error('workload ' + file + ': ' + e.message);
    process.exitCode = 1;
    continue;
  }
  const rel = path.relative(dir, file).split(path.sep).join('/');
  const value = typeof r.value === 'number' ? r.value : 'NA';
  console.log('bench\t' + MODE + '\t' + rel + '\t' + r.ms.toFixed(4) + '\t' + value);
}
