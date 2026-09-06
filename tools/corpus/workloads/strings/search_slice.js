// indexOf/slice/substring on a working string.
function bench() {
  var hay = "";
  for (var i = 0; i < 200; i++) {
    hay += "ab-cd-";
  }
  var n = 0;
  for (var i = 0; i < 100000; i++) {
    var idx = hay.indexOf("cd", i % 400);
    n += idx;
    n += hay.slice(0, (i % 400) + 1).length;
  }
  return n;
}
bench();
