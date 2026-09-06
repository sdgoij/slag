// Map get/set/has/delete churn over a bounded map.
function bench() {
  var m = new Map();
  for (var i = 0; i < 128; i++) {
    m.set(i, i);
  }
  var s = 0;
  for (var i = 0; i < 300000; i++) {
    var k = i & 127;
    if (m.has(k)) {
      s += m.get(k);
    }
    m.set(k, i & 255);
  }
  return s;
}
bench();
