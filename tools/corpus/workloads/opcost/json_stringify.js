// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var o2 = { a: 1, b: 2, c: 3 };
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    s = (s + JSON.stringify(o2).length) | 0;
  }
  return s;
}
bench();
