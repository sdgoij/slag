// Per-code-unit string reads (charCodeAt on a medium string).
function bench() {
  var s = "";
  for (var i = 0; i < 256; i++) {
    s += String.fromCharCode(32 + (i % 90));
  }
  var n = 0;
  for (var i = 0; i < 300000; i++) {
    n += s.charCodeAt(i & 255);
  }
  return n;
}
bench();
