// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var arr = [1, 2, 3];
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    arr.length = 0; s = (s + arr.length) | 0;
  }
  return s;
}
bench();
