// opcost row: one operation inside a bare loop. See gen_opcost.js.
function bench() {
  var MASK = 1023;
  var arr = new Array(1024);
  // Not arr[i] = i on purpose: element_read's value would then equal
  // baseline's, and a mis-wired body would slip past the parity check.
  for (var i = 0; i < 1024; i++) arr[i] = 1024 - i;
  var m = new Map();
  for (var i = 0; i < 1024; i++) m.set(i, i);
  var set = new Set();
  for (var i = 0; i < 1024; i++) set.add(i);
  var o = { x: 7 };
  var obj = { m: function (x) { return x + 1; } };
  var f = function (x) { return x + 1; };
  var strs = ["abcdefgh", "ijklmnop", "qrstuvwx", "yz012345"];
  var re = /[a-z]+/;
  var o2 = { a: 1, b: 2, c: 3 };
  var o3 = { a: 1, b: 2, c: 3, d: 4 };
  var pushArr = [];
  var s = 0;
  for (var i = 0; i < 100000; i++) {
    var k = i & MASK;
    s = (s + o.x) | 0;
  }
  return s;
}
bench();
