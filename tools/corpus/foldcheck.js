// Force one workload's timed function into TurboFan under d8, so
// --print-opt-code dumps it and the caller can decide from the machine code
// whether the work is still there.
//
// One workload per process, deliberately: the dump then contains only this
// workload's functions, so the caller's parser can scan the whole output
// without having to associate disassembly sections with rows.
//
// Usage:
//   d8 --allow-natives-syntax --print-opt-code foldcheck.js -- <file.js>
//
// Prints nothing on success — stdout is the disassembly the caller parses.
// That is why the CALLER owns the --print-opt-code flag: d8 writes the dump
// to stdout as it optimizes, and interleaving our own output with it would
// make the dump unparseable.
//
// Must stay sloppy: a direct eval() loads the workload's `function bench`
// into this script's scope.

if (arguments.length < 1) {
  printErr('usage: d8 --allow-natives-syntax --print-opt-code foldcheck.js -- <file.js>');
  quit(1);
}

const file = String(arguments[0]);
const source = read(file).replace(/\s+$/, '');
const callAt = source.lastIndexOf('bench(');
if (callAt < 0) {
  printErr('foldcheck: no bench(...) invocation in ' + file);
  quit(1);
}

const def = source.slice(0, callAt);
const argsSrc = source.slice(callAt + 'bench('.length, source.length - 2);
eval(def); // eslint-disable-line no-eval
globalThis.__bench = bench;
const args = eval('[' + argsSrc + ']'); // eslint-disable-line no-eval
const fn = globalThis.__bench;

// Warm, then force TurboFan. Forcing matters: d8's own tier-up thresholds
// would otherwise decide whether anything is dumped at all, and a short
// diagnostic run is not long enough to rely on them.
fn.apply(null, args);
fn.apply(null, args);
%PrepareFunctionForOptimization(fn);
fn.apply(null, args);
%OptimizeFunctionOnNextCall(fn);
fn.apply(null, args);

// A second forced call covers the case where the first optimized call
// deoptimized and left no (or a stale) dump.
%OptimizeFunctionOnNextCall(fn);
fn.apply(null, args);
