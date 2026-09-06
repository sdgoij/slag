// Template-literal and string-coercion building.
function bench() {
  var s = 0;
  for (var i = 0; i < 300000; i++) {
    var t = "value=" + i + ":" + (i * 2);
    s += t.length;
  }
  return s;
}
bench();
