// Warm stores to a stable single object (o.x = i churn).
function bench() {
  var o = { a: 0, b: 0, c: 0, d: 0 };
  var s = 0;
  for (var i = 0; i < 4000000; i++) {
    o.d = i;
    s += o.d;
  }
  return s;
}
bench();
