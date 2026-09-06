// Typed-array element stores and reads (typed-array write machinery).
function bench() {
  var ta = new Uint8Array(65536);
  var s = 0;
  for (var k = 0; k < 2000000; k++) {
    ta[k & 65535] = k & 255;
    s += ta[k & 65535];
  }
  return s;
}
bench();
