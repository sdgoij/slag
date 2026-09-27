// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  function P() {}
  P.prototype.get = function (x) { return x + 1; };
  var p = new P();
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    s = (s + p.get(k)) | 0;
  }
  return s;
}
bench();
