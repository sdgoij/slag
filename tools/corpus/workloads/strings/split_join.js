// split/join round trips over a small token string.
function bench() {
  var s = "a,b,c,d,e,f,g,h";
  var n = 0;
  for (var i = 0; i < 60000; i++) {
    var parts = s.split(",");
    parts[0] = "z";
    n += parts.join(",").length;
  }
  return n;
}
bench();
