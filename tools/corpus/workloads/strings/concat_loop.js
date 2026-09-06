// String concatenation growth (rope appends).
function bench() {
  var s = "";
  var n = 0;
  for (var i = 0; i < 200000; i++) {
    s += "x";
    n += s.length;
  }
  return n;
}
bench();
