// for-in over an object's own enumerable keys.
function bench() {
  var o = { a: 1, b: 2, c: 3, d: 4, e: 5 };
  var s = 0;
  for (var r = 0; r < 150000; r++) {
    for (var k in o) {
      s += o[k];
    }
  }
  return s;
}
bench();
