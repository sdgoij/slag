// Set.add/has churn over a bounded set.
function bench() {
  var st = new Set();
  for (var i = 0; i < 128; i++) {
    st.add(i);
  }
  var s = 0;
  for (var i = 0; i < 600000; i++) {
    var k = i & 127;
    if (st.has(k)) {
      s += 1;
    }
    st.add(k);
  }
  return s;
}
bench();
