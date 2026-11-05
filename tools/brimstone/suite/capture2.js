var t0 = Date.now();
function bench() {
  var x = 1;
  function f() { return x; }
  var s = 0;
  for (var i = 0; i < 2000000; i++) s += f();
  return s;
}
var v = bench();
console.log('capture2 ' + (Date.now() - t0) + ' ' + (v > 0));
