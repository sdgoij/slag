// A record literal past the in-object field cap (INLINE_FIELDS is 8): the ninth
// key's descriptor ordinal is 8, which is where the property vector starts for a
// suffix store. The objects escape into a module-level sink, so neither engine
// can scalar-replace the create away, and the reads cover one key below and one
// above the cap.
var SINK = new Array(64);

function bench() {
  var s = 0;
  for (var i = 0; i < 200000; i++) {
    var o = {
      a: i, b: i, c: i, d: i, e: i, f: i, g: i, h: i, j: i, k: i, l: i, m: i,
    };
    SINK[i & 63] = o;
    s = (s + o.a + o.k) | 0;
  }
  for (var i = 0; i < 64; i++) {
    s = (s + SINK[i].m) | 0;
  }
  return s;
}
bench();
