// Self-recursion (fib) mixed with a counting loop.
function bench() {
  function fib(n) {
    if (n < 2) {
      return n;
    }
    return fib(n - 1) + fib(n - 2);
  }
  var s = 0;
  for (var i = 0; i < 1200; i++) {
    s += fib(i % 20);
  }
  return s;
}
bench();
