// push/pop churn on a bounded array (dense append + truncate).
function bench() {
  var a = [];
  var s = 0;
  for (var i = 0; i < 60000; i++) {
    a.push(i);
    if (a.length === 64) {
      while (a.length > 0) {
        s += a.pop();
      }
    }
  }
  return s;
}
bench();
