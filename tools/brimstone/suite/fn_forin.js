var t0 = Date.now();
function bench() {
  var o = {};
  for (var i = 0; i < 32; i++) o['k' + i] = i;
  var s = 0;
  for (var j = 0; j < 20000; j++) {
    for (var k in o) s += o[k];
  }
  return s;
}
var v = bench();
console.log('fn_forin ' + (Date.now() - t0) + ' ' + (v > 0));
