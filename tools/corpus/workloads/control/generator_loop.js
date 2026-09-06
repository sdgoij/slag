// Generator-driven counter (generator resume machinery).
function bench() {
  function* counter() {
    var i = 0;
    while (true) {
      yield i++;
    }
  }
  var s = 0;
  var g = counter();
  for (var i = 0; i < 200000; i++) {
    s += g.next().value;
  }
  return s;
}
bench();
