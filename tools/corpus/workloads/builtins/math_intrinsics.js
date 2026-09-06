// Math + number-formatting intrinsic calls.
function bench() {
  var s = 0;
  for (var i = 0; i < 600000; i++) {
    s += Math.floor(Math.sqrt(i * i));
    s += Math.abs(i - 500000);
  }
  return s;
}
bench();
