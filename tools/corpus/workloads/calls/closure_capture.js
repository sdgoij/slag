// Closures capturing an outer variable, called in a loop.
function bench() {
  var base = 1;
  var fns = [];
  for (var i = 0; i < 64; i++) {
    fns.push((function (k) {
      return function (x) {
        return base + k + x;
      };
    })(i));
  }
  var s = 0;
  for (var i = 0; i < 1000000; i++) {
    s += fns[i & 63](i);
  }
  return s;
}
bench();
