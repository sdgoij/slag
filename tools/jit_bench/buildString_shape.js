// buildString shape — slag JIT comparison for the --bench suite (function-wrapped
// certified shape; warmup call compiles loop bodies, then 10 unrolled
// timed calls are averaged).
function bench() { var a = []; var l = 0; var c = 0; for (var i = 0; i < 3_000_000; i++) { a[l++] = i; if (l === 10000) { c++; a.length = l = 0; } } return c; }
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
console.log("buildString shape " + ((t1 - t0) / 10).toFixed(2) + "ms ok=true result=" + res);
