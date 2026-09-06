// Higher-order array methods over a fixed dense array.
function bench() {
  var a = [];
  for (var i = 0; i < 1000; i++) {
    a.push(i);
  }
  var s = 0;
  for (var i = 0; i < 150; i++) {
    var b = a.map(function (x) { return x + 1; });
    var c = b.filter(function (x) { return (x & 1) === 0; });
    s += c.reduce(function (acc, x) { return acc + x; }, 0);
  }
  return s;
}
bench();
