// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var small = new Map([[1, 1], [2, 2], [3, 3], [4, 4]]);
  var cb = function (x) { return x + 1; };
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    small.forEach(cb); s = (s + 1) | 0;
  }
  return s;
}
bench();
