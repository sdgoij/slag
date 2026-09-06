// Object.keys/values/entries over a fixed-size object.
function bench() {
  var o = { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6 };
  var s = 0;
  for (var i = 0; i < 20000; i++) {
    var ks = Object.keys(o);
    var vs = Object.values(o);
    var es = Object.entries(o);
    s += ks.length + vs.length + es.length;
  }
  return s;
}
bench();
