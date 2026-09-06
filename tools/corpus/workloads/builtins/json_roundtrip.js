// JSON.stringify + JSON.parse round trips on a small nested object.
function bench() {
  var s = 0;
  for (var i = 0; i < 15000; i++) {
    var obj = { id: i, name: "n" + (i % 100), tags: [i, i + 1, i + 2], inner: { x: i } };
    var text = JSON.stringify(obj);
    var back = JSON.parse(text);
    s += back.inner.x;
  }
  return s;
}
bench();
