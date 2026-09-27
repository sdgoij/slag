// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var strs = ["abcdefgh", "ijklmnop", "qrstuvwx", "yz012345"];
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    s = (s + strs[k & 3].charCodeAt(k & 7)) | 0;
  }
  return s;
}
bench();
