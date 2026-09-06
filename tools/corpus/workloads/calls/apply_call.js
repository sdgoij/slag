// f.apply(null, arr) with a small dense array.
function bench() {
  function add(a, b, c, d) {
    return a + b + c + d;
  }
  var args = [1, 2, 3, 4];
  var s = 0;
  for (var i = 0; i < 1000000; i++) {
    s += add.apply(null, args);
  }
  return s;
}
bench();
