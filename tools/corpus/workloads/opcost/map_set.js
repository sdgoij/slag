// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var m = new Map();
  for (var i = 0; i < 1024; i++) m.set(i, i);
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    m.set(k, i); s = (s + 1) | 0;
  }
  return s;
}
bench();
