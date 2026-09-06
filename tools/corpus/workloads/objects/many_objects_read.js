// Reads across many distinct same-shape objects (cycling working set past
// the direct-mapped read cells).
function bench() {
  var os = [];
  for (var i = 0; i < 1024; i++) {
    os.push({ x: i, y: i, z: i, w: i });
  }
  var s = 0;
  for (var i = 0; i < 2000000; i++) {
    s += os[i & 1023].w;
  }
  return s;
}
bench();
