var t0 = Date.now();
function bench() {
  function f() { var y = 1; return y + y + y + y + y + y + y + y; }
  var s = 0;
  for (var i = 0; i < 2000000; i++) s += f();
  return s;
}
var v = bench();
console.log('local ' + (Date.now() - t0) + ' ' + (v > 0));
