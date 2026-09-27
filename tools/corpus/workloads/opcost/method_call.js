// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var obj = { m: function (x) { return x + 1; } };
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    s = (s + obj.m(k)) | 0;
  }
  return s;
}
bench();
