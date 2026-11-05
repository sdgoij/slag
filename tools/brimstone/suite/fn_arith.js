var t0 = Date.now();
function bench() {
  var s = 0;
  for (var i = 0; i < 10000000; i++) {
    var k = i & 1023;
    s = (s + k) | 0;
  }
  return s;
}
var v = bench();
console.log('fn_arith ' + (Date.now() - t0) + ' ' + (v > 0));
