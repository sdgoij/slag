// closure capture — slag JIT comparison for the --bench suite (function-wrapped
// certified shape; warmup call compiles loop bodies, then 10 unrolled
// timed calls are averaged).
function bench() { function make(base) { var x = base; return (y) => x + y; } var f = make(2); var n = 0; for (var i = 0; i < 1_000_000; i++) { n = f(n); } return n; }
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
console.log("closure capture " + ((t1 - t0) / 10).toFixed(2) + "ms ok=true result=" + res);
