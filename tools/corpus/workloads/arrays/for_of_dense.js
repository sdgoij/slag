// for-of iteration over a dense array (iterator-protocol fast path).
function bench() {
  var a = [];
  for (var i = 0; i < 10000; i++) {
    a.push(i);
  }
  var s = 0;
  for (var r = 0; r < 300; r++) {
    for (var v of a) {
      s += v;
    }
  }
  return s;
}
bench();
