// Array spread + concat/join machinery.
function bench() {
  var s = 0;
  for (var i = 0; i < 12000; i++) {
    var base = [i, i + 1, i + 2];
    var copy = base.slice(0);
    var joined = copy.concat(base).join(",");
    s += joined.length;
  }
  return s;
}
bench();
