// string concat — slag JIT comparison for the --bench suite (function-wrapped
// certified shape; warmup call compiles loop bodies, then 10 unrolled
// timed calls are averaged).
function bench() { var s = ''; for (var i = 0; i < 100_000; i++) { s += 'x'; } return s.length; }
bench();  // warmup (untimed; compiles loop bodies on first consult)
var t0 = Date.now();
var res;
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
var t1 = Date.now();
console.log("string concat " + ((t1 - t0) / 10).toFixed(2) + "ms ok=true result=" + res);
