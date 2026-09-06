// Amplified test262 fixture: test262/test/language/statements/try/completion-values.js
// body iterations per bench() call: 1423 (~0.035 ms/iter under node)

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


assert.sameValue(
  eval('99; do { -99; try { 39 } catch (e) { -1 } finally { 42; break; -2 }; } while (false);'),
  42
);
assert.sameValue(
  eval('99; do { -99; try { [].x.x } catch (e) { -1; } finally { 42; break; -3 }; } while (false);'),
  42
);
assert.sameValue(
  eval('99; do { -99; try { 39 } catch (e) { -1 } finally { break; -2 }; } while (false);'),
  undefined
);
assert.sameValue(
  eval('99; do { -99; try { [].x.x } catch (e) { -1; } finally { break; -3 }; } while (false);'),
  undefined
);
assert.sameValue(
  eval('99; do { -99; try { 39 } catch (e) { -1 } finally { 42; break; -3 }; -77 } while (false);'),
  42
);
assert.sameValue(
  eval('99; do { -99; try { [].x.x } catch (e) { -1; } finally { 42; break; -3 }; -77 } while (false);'),
  42
);
assert.sameValue(
  eval('99; do { -99; try { 39 } catch (e) { -1 } finally { break; -3 }; -77 } while (false);'),
  undefined
);
assert.sameValue(
  eval('99; do { -99; try { [].x.x } catch (e) { -1; } finally { break; -3 }; -77 } while (false);'),
  undefined
);
assert.sameValue(
  eval('99; do { -99; try { 39 } catch (e) { -1 } finally { 42; continue; -3 }; } while (false);'),
  42
);
assert.sameValue(
  eval('99; do { -99; try { [].x.x } catch (e) { -1; } finally { 42; continue; -3 }; } while (false);'),
  42
);
assert.sameValue(
  eval('99; do { -99; try { 39 } catch (e) { -1 } finally { 42; continue; -3 }; -77 } while (false);'),
  42
);
assert.sameValue(
  eval('99; do { -99; try { [].x.x } catch (e) { -1 } finally { 42; continue; -3 }; -77 } while (false);'),
  42
);
  }
  for (var __t262Iter = 0; __t262Iter < 1423; __t262Iter++) {
    __t262Body();
  }
  return 1;
}
bench();
