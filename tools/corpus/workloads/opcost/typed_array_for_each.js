// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var ta = new Uint8Array([1, 2, 3, 4]);
  var cb = function (x) { return x + 1; };
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    ta.forEach(cb); s = (s + 1) | 0;
  }
  return s;
}
bench();
