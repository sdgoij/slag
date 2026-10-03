// SpiderMonkey (js shell) runner for the workload corpus — the js-shell
// mirror of run_d8.js.
//
// Usage:
//   js run_jsshell.js <mode> <root-dir> <file.js>...
//
// Three things differ from run_d8.js, all forced by the shell:
//   - SpiderMonkey exposes the script arguments as `scriptArgs`, not
//     `arguments`, and does not treat `--` specially (it appears verbatim),
//     so the caller passes the args directly after the script.
//   - The shell has no directory-listing API, so the caller supplies the file
//     list (the driver globs it).
//   - The engine flags are not visible to the script, so the mode is an
//     argument (SM's tiers are selected by shell flags, not by this runner).
//
// Everything else mirrors bench_once's protocol exactly: the definition is
// evaluated once, the arguments are bound once, the function is warmed with 2
// calls, then 3 timed samples report the MIN per-call time. A sample is
// batched to clear a ~20ms floor. Prints one line per workload:
//
//   bench<TAB><mode><TAB><relpath><TAB><ms><TAB><result>
//
// This file must stay sloppy: a direct eval() loads each workload's
// `function bench` into this script's scope.

const WARMUP = 2;
const TIMED = 3;
const MAX_REPS = 1 << 20;
const FLOOR_MS = 20;

const argv = scriptArgs;
if (argv.length < 2) {
  print('usage: js run_jsshell.js <mode> <root> <file.js>...');
  quit(1);
}
const mode = String(argv[0]);
const root = String(argv[1]).replace(/[\\/]+$/, '').replace(/\\/g, '/');

function relPath(file) {
  let rel = String(file).replace(/\\/g, '/');
  let at = rel.indexOf(root + '/');
  if (at >= 0) {
    rel = rel.slice(at + root.length + 1);
  }
  return rel;
}

function benchOnce(source) {
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
