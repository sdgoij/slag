var t0 = Date.now();
function bench() {
  var a = [];
  for (var i = 0; i < 200000; i++) a[i] = i;
  var s = 0;
  for (var j = 0; j < 10; j++) for (var i = 0; i < 200000; i++) s += a[i];
  return s;
}
var v = bench();
console.log('fn_array ' + (Date.now() - t0) + ' ' + (v > 0));
