// array iteration — slag JIT comparison for the --bench suite (function-wrapped
// certified shape; warmup call compiles loop bodies, then 10 unrolled
// timed calls are averaged).
function bench() { var a = [1,2,3,4,5,6,7,8,9,10]; var n = 0; for (var i = 0; i < 100_000; i++) { for (var v of a) { n += v; } } return n; }
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
console.log("array iteration " + ((t1 - t0) / 10).toFixed(2) + "ms ok=true result=" + res);
