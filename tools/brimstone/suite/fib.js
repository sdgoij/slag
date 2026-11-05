var t0 = Date.now();
function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
var v = fib(30);
console.log('fib ' + (Date.now() - t0) + ' ' + v);
