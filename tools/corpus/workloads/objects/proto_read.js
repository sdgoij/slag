// Prototype-chain data reads (one link; the chain resolution cache path).
function bench() {
  var proto = { m: 2, n: 3 };
  var o = Object.create(proto);
  o.own = 1;
  var s = 0;
  for (var i = 0; i < 2000000; i++) {
    s += o.m + o.n;
  }
  return s;
}
bench();
