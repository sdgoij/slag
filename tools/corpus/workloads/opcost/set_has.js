// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var set = new Set();
  for (var i = 0; i < 1024; i++) set.add(i);
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    s = (s + (set.has(k) ? 1 : 0)) | 0;
  }
  return s;
}
bench();
