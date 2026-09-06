// Amplified test262 fixture: test262/test/language/statements/for-in/head-let-fresh-binding-per-iteration.js
// body iterations per bench() call: 100000 (~0.000 ms/iter under node)

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

function bench() {
  function __t262Body() {


var fns = {};
var obj = Object.create(null);
obj.a = 1;
obj.b = 1;
obj.c = 1;

for (let x in obj) {
  // Store function objects as properties of an object so that their return
  // value may be verified regardless of the for-in statement's enumeration
  // order.
  fns[x] = function() { return x; };
}

assert.sameValue(typeof fns.a, 'function', 'property definition: "a"');
assert.sameValue(fns.a(), 'a');
assert.sameValue(typeof fns.b, 'function', 'property definition: "b"');
assert.sameValue(fns.b(), 'b');
assert.sameValue(typeof fns.c, 'function', 'property definition: "c"');
assert.sameValue(fns.c(), 'c');
  }
  for (var __t262Iter = 0; __t262Iter < 100000; __t262Iter++) {
    __t262Body();
  }
  return 1;
}
bench();
