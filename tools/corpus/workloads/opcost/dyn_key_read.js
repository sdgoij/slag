// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var strs = ["abcdefgh", "ijklmnop", "qrstuvwx", "yz012345"];
  var props = { abcdefgh: 1, ijklmnop: 2, qrstuvwx: 3, yz012345: 4 };
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    s = (s + props[strs[k & 3]]) | 0;
  }
  return s;
}
bench();
