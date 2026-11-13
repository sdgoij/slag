// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    var f4 = { a: 1, b: 2, c: 3, d: 4 }; s = (s + Object.keys(f4).length) | 0;
  }
  return s;
}
bench();
