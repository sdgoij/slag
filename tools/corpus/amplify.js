// test262 → workload amplifier (prototype).
//
// Picks plain, self-contained test262 fixtures (no frontmatter gating beyond
// the sloppy default), wraps the fixture body in a repetition loop inside
// `function bench()`, calibrates the iteration count so one bench() call runs
// ~150ms under node, and writes the result under --out mirroring the fixture
// path. The fixture's own assert calls guard correctness: a divergent engine
// throws during bench() and the workload run fails loudly.
//
// Usage:
//   node amplify.js --out <workloads-dir> [--slag <slag-binary>] <fixture.js>...
//
// Only fixtures that parse frontmatter cleanly and then run to completion
// under BOTH node and slag (exit 0) are kept; everything else is skipped with
// a reason on stderr.
'use strict';

const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { spawnSync } = require('node:child_process');

const node = process.execPath;
let slag = process.env.SLAG_BIN || path.join(__dirname, '..', '..', 'target', 'release', 'slag.exe');
let outDir = null;

const argv = process.argv.slice(2);
const positional = [];
for (let i = 0; i < argv.length; i++) {
  if (argv[i] === '--out') {
    outDir = argv[++i];
  } else if (argv[i] === '--slag') {
    slag = argv[++i];
  } else {
    positional.push(argv[i]);
  }
}
if (!outDir || positional.length === 0) {
  console.error('usage: node amplify.js --out <workloads-dir> [--slag <binary>] <fixture.js>...');
  process.exit(1);
}

// Minimal assert shim (test262 harness/assert.js subset). Placed at the
// top of every generated workload, OUTSIDE the timed bench(), so the timed
// region is the fixture body itself.
const SHIM = `
var assert = {};
(function () {
  function is(x, y) {
    if (x === y) {
      return x !== 0 || 1 / x === 1 / y;
    }
    return x !== x && y !== y;
  }
  function fail(msg) {
    throw new Error('assert: ' + msg);
  }
  function sameValue(actual, expected, msg) {
    if (!is(actual, expected)) {
      fail('sameValue: expected ' + expected + ', got ' + actual + (msg ? ' (' + msg + ')' : ''));
    }
  }
  function throws(expected, fn, msg) {
    var threw = false;
    try {
      fn();
    } catch (e) {
      threw = true;
      var nameOk = e && e.name && expected && expected.name === e.name;
      var ctorOk = false;
      try { ctorOk = e instanceof expected; } catch (ignored) {}
      if (!ctorOk && !nameOk) {
        fail('throws: expected ' + expected.name + ', got ' + e);
      }
      return;
    }
    if (!threw) {
      fail('throws: expected ' + expected.name + ' but nothing was thrown' + (msg ? ' (' + msg + ')' : ''));
    }
  }
  function compareArray(actual, expected, msg) {
    if (actual.length !== expected.length) {
      fail('compareArray: length ' + actual.length + ' !== ' + expected.length);
    }
    for (var i = 0; i < actual.length; i++) {
      if (!is(actual[i], expected[i])) {
        fail('compareArray: index ' + i + ' differs');
      }
    }
  }
  assert.sameValue = sameValue;
  assert.notSameValue = function (a, b, msg) {
    if (is(a, b)) { fail('notSameValue: expected different, both ' + a); }
  };
  assert.throws = throws;
  assert.compareArray = compareArray;
  assert.eq = sameValue;
  assert.true = function (v, msg) {
    if (v !== true) { fail('true: got ' + v); }
  };
  assert.false = function (v, msg) {
    if (v !== false) { fail('false: got ' + v); }
  };
})();
`;

const ALLOWED_METHODS = new Set(['sameValue', 'notSameValue', 'true', 'false', 'throws', 'compareArray', 'eq']);

function parseFixture(file) {
  const text = fs.readFileSync(file, 'utf8');
  const start = text.indexOf('/*---');
  if (start < 0) {
    return { skip: 'no frontmatter' };
  }
  const end = text.indexOf('---*/', start);
  if (end < 0) {
    return { skip: 'unterminated frontmatter' };
  }
  const fm = text.slice(start + 5, end);
  const key = (k) => new RegExp('^\\s*' + k + '\\s*:', 'm').test(fm);
  if (key('negative') || key('includes') || key('features')) {
    return { skip: 'frontmatter gates on negative/includes/features' };
  }
  const flagsMatch = /^\s*flags:\s*\[([^\]]*)\]/m.exec(fm);
  const flags = flagsMatch ? flagsMatch[1].split(',').map((s) => s.trim()).filter(Boolean) : [];
  for (const f of flags) {
    if (f === 'module' || f === 'raw' || f === 'async' || f === 'generated' || f === 'onlyStrict') {
      return { skip: 'flag not supported: ' + f };
    }
  }
  const body = text.slice(end + 5);
  if (/\$262|\bTest262Error\b|print\s*\(|verifyProperty/.test(body)) {
    return { skip: 'body needs host/harness globals' };
  }
  const used = body.match(/assert\.(\w+)/g);
  if (!used) {
    return { skip: 'no assert usage (nothing guards correctness)' };
  }
  for (const u of used) {
    if (!ALLOWED_METHODS.has(u.slice('assert.'.length))) {
      return { skip: 'assert method not in shim: ' + u };
    }
  }
  return { fm, flags, body };
}

function benchBody(body, iter) {
  return (
    'function bench() {\n' +
    '  function __t262Body() {\n' +
    body.replace(/\n$/, '') + '\n' +
    '  }\n' +
    '  for (var __t262Iter = 0; __t262Iter < ' + iter + '; __t262Iter++) {\n' +
    '    __t262Body();\n' +
    '  }\n' +
    '  return 1;\n' +
    '}\n' +
    'bench();\n'
  );
}

// Measure one body iteration under node with an adaptive count (Date.now has
// ~1ms resolution, so grow K until the run is comfortably above it).
function calibrate(body) {
  const probe = (k) =>
    SHIM +
    '\nconst { performance } = require("node:perf_hooks");\n' +
    'function bench() {\n' +
    '  function __t262Body() {\n' +
    body.replace(/\n$/, '') + '\n' +
    '  }\n' +
    '  var t0 = performance.now();\n' +
    '  for (var i = 0; i < ' + k + '; i++) {\n' +
    '    __t262Body();\n' +
    '  }\n' +
    '  console.log(performance.now() - t0);\n' +
    '}\n' +
    'bench();\n';
  const tmp = path.join(os.tmpdir(), 't262-probe-' + process.pid + '.js');
  let k = 256;
  for (;;) {
    fs.writeFileSync(tmp, probe(k));
    const res = spawnSync(node, [tmp], { encoding: 'utf8', timeout: 30000 });
    if (res.error || res.status === null) {
      fs.unlinkSync(tmp);
      return { fail: 'probe spawn failed: ' + (res.error ? res.error.message : 'timeout at k=' + k) };
    }
    if (res.status !== 0) {
      fs.unlinkSync(tmp);
      return { fail: 'body fails under node: ' + (res.stderr || res.stdout || '').trim().slice(0, 300) };
    }
    const ms = parseFloat(res.stdout);
    if (ms >= 8 || k >= 1048576) {
      fs.unlinkSync(tmp);
      const perIter = ms / k;
      const iter = Math.max(1, Math.min(100000, Math.round(50 / perIter)));
      return { iter, perIter };
    }
    k *= 2;
  }
}

function run(cmd, args) {
  const res = spawnSync(cmd, args, { encoding: 'utf8' });
  if (res.error) {
    return { error: res.error.message };
  }
  return { status: res.status, out: res.stdout, err: res.stderr };
}

let kept = 0;
for (const fixture of positional) {
  const parsed = parseFixture(fixture);
  if (parsed.skip) {
    console.error('skip ' + fixture + ': ' + parsed.skip);
    continue;
  }
  const { body } = parsed;
  console.error('calibrating ' + fixture + ' ...');
  const cal = calibrate(body);
  if (cal.fail) {
    console.error('skip ' + fixture + ': ' + cal.fail);
    continue;
  }
  const source = '// Amplified test262 fixture: ' + fixture + '\n' +
    '// body iterations per bench() call: ' + cal.iter + ' (~' + cal.perIter.toFixed(3) + ' ms/iter under node)\n' +
    SHIM + '\n' + benchBody(body, cal.iter);

  const rel = path.relative('test262/test', fixture).split(path.sep).join('/');
  const target = path.join(outDir, rel);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, source);

  const nodeRes = run(node, [target]);
  if (nodeRes.status !== 0) {
    console.error('drop ' + fixture + ': node run failed: ' + (nodeRes.err || '').trim().slice(0, 300));
    fs.unlinkSync(target);
    continue;
  }
  const slagRes = run(slag, [target]);
  if (slagRes.status !== 0) {
    console.error('drop ' + fixture + ': slag run failed: ' + (slagRes.err || '').trim().slice(0, 300));
    fs.unlinkSync(target);
    continue;
  }
  kept += 1;
  console.log('kept ' + rel + '  iter=' + cal.iter);
}
console.error('kept ' + kept + ' of ' + positional.length);
