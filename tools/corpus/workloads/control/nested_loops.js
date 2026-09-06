// Nested counting loops with inner accumulation.
function bench() {
  var s = 0;
  for (var i = 0; i < 2000; i++) {
    for (var j = 0; j < 2000; j++) {
      s += (i * 31 + j) % 7;
    }
  }
  return s;
}
bench();
