// Direct calls to a leaf function (certified leaf-call path).
function bench() {
  function leaf(x) {
    return x + 1;
  }
  var s = 0;
  for (var i = 0; i < 2000000; i++) {
    s += leaf(i);
  }
  return s;
}
bench();
