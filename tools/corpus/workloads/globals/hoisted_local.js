// The same declarative binding as `declarative_read.js`, read once into a local
// before the loop: the workaround the cell gap implies, and the floor the other
// two rows are compared against.
const K = 3;

function bench() {
  var k = K;
  var s = 0;
  for (var i = 0; i < 1000000; i++) {
    s += i * 3 + k;
  }
  return s;
}
bench();
