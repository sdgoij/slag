// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var o3 = { a: 1, b: 2, c: 3, d: 4 };
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    s = (s + Object.keys(o3).length) | 0;
  }
  return s;
}
bench();
