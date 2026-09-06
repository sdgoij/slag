// own-data property reads on a single hot object (warm value-cell path).
function bench() {
  var o = { a: 1, b: 2, c: 3, d: 4 };
  var s = 0;
  for (var i = 0; i < 4000000; i++) {
    s += o.a + o.b;
  }
  return s;
}
bench();
