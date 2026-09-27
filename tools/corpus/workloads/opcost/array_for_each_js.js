// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var small = [1, 2, 3, 4];
  var cb = function (x) { return x + 1; };
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    s = (s + cb(small[k & 3]) + cb(small[(k + 1) & 3]) + cb(small[(k + 2) & 3]) + cb(small[(k + 3) & 3])) | 0;
  }
  return s;
}
bench();
