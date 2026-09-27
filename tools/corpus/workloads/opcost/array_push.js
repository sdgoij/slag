// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var pushArr = [];
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    pushArr.push(k); if (pushArr.length > 4096) pushArr.length = 0; s = (s + 1) | 0;
  }
  return s;
}
bench();
