// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var small = [4, 2, 3, 1];
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    s = (s + small.toSorted().length) | 0;
  }
  return s;
}
bench();
