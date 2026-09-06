// Plain index reads/writes over a preallocated dense array.
function bench() {
  var a = new Array(200000);
  for (var i = 0; i < 200000; i++) {
    a[i] = i;
  }
  var s = 0;
  for (var i = 0; i < 3000000; i++) {
    s += a[i & 199999];
  }
  return s;
}
bench();
