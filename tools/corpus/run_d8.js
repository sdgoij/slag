// d8 runner for the workload corpus — the d8 mirror of run_node.js.
//
// Usage:
//   d8 [--jitless] run_d8.js -- <mode> <root-dir> <file.js>...
//
// Two things differ from run_node.js, both forced by the shell:
//   - d8 exposes no directory-listing API, so the caller supplies the file
//     list (the driver globs it).
//   - d8 does not expose the engine flags to the script, so the mode is
//     passed as an argument instead of being read from process.execArgv.
//
// Everything else mirrors bench_once's protocol exactly: the definition is
// evaluated once, the arguments are bound once, the function is warmed with 2
// calls, then 3 timed samples report the MIN per-call time. A sample is
// batched so it clears a ~20ms floor — a single call can be far below that
// (V8 closed-form-folds affine loops, so a per-call time is near-constant in
// the iteration count), and timing one call at that scale would be
// quantization-dominated. Prints one line per workload:
//
//   bench<TAB><mode><TAB><relpath><TAB><ms><TAB><result>
//
// This file must stay sloppy: a direct eval() loads each workload's
// `function bench` into this script's scope, and a 'use strict' directive
// would scope the eval's declarations to the eval itself.

const WARMUP = 2;
const TIMED = 3;
const MAX_REPS = 1 << 20;
const FLOOR_MS = 20;

const argv = arguments;
if (argv.length < 2) {
  print('usage: d8 run_d8.js -- <mode> <root> <file.js>...');
  quit(1);
}
const mode = argv[0];
const root = String(argv[1]).replace(/[\\/]+$/, '').replace(/\\/g, '/');

function relPath(file) {
  let rel = String(file).replace(/\\/g, '/');
  if (rel.indexOf(root + '/') === 0) {
    rel = rel.slice(root.length + 1);
  }
  return rel;
}

// Split `function bench(...) {...} bench(ARGS);` at the LAST `bench(` so the
// definition is evaluated once and the args are bound once (same as
// bench_once).
function benchOnce(source) {
  // The arg split assumes the source ends `);` — trim trailing whitespace so
  // files ending with a newline after `bench();` split identically to rows.
  const trimmed = source.replace(/\s+$/, '');
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
    fn.apply(null, args);
  }
  let reps = 1;
  {
    const t0 = performance.now();
    fn.apply(null, args);
    const single = performance.now() - t0;
    if (single > 0 && single < FLOOR_MS) {
      reps = Math.min(MAX_REPS, Math.max(1, Math.ceil(FLOOR_MS / single)));
    }
  }
  if (reps > 1) {
    for (let w = 0; w < reps; w++) {
      fn.apply(null, args);
    }
  }
  let best = Infinity;
  let value;
  for (let t = 0; t < TIMED; t++) {
    const t0 = performance.now();
    for (let r = 0; r < reps; r++) {
      value = fn.apply(null, args);
    }
    const perCall = (performance.now() - t0) / reps;
    if (perCall < best) {
      best = perCall;
    }
  }
  return { ms: best, value };
}

let failures = 0;
for (let i = 2; i < argv.length; i++) {
  const file = String(argv[i]);
  let source;
  try {
    source = read(file);
  } catch (e) {
    print('read failed ' + file + ': ' + e);
    failures++;
    continue;
  }
  if (!source.trim()) {
    continue;
  }
  let r;
  try {
    r = benchOnce(source);
  } catch (e) {
    print('workload ' + file + ': ' + e);
    failures++;
    continue;
  }
  const value = typeof r.value === 'number' ? r.value : 'NA';
  print('bench\t' + mode + '\t' + relPath(file) + '\t' + r.ms.toFixed(4) + '\t' + value);
}
if (failures > 0) {
  quit(1);
}
