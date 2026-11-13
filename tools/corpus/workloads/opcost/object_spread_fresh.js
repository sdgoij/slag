// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    var f5 = { a: 1, b: 2, c: 3, d: 4 }; var g5 = { ...f5 }; s = (s + g5.a) | 0;
  }
  return s;
}
bench();
