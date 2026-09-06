// Compound member updates (o.x += 1 read-modify-write).
function bench() {
  var o = { x: 0 };
  var s = 0;
  for (var i = 0; i < 4000000; i++) {
    o.x += 1;
    s += o.x;
  }
  return s;
}
bench();
