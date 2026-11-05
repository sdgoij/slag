var t0 = Date.now();
function bench() {
  var x = 1;
  function f() { return x + x + x + x + x + x + x + x; }
  var s = 0;
  for (var i = 0; i < 2000000; i++) s += f();
  return s;
}
var v = bench();
console.log('capture ' + (Date.now() - t0) + ' ' + (v > 0));
