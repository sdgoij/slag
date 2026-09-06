// Object destructuring of freshly created objects.
function bench() {
  var s = 0;
  for (var i = 0; i < 1000000; i++) {
    var o = { a: i, b: i + 1, c: { d: i + 2 } };
    var a = o.a, b = o.b, inner = o.c;
    var d = inner.d;
    s += a + b + d;
  }
  return s;
}
bench();
