// per-iteration — slag JIT comparison for the --bench suite (function-wrapped
// certified shape; warmup call compiles loop bodies, then 10 unrolled
// timed calls are averaged).
function bench() { function makeFns() { var fns = []; for (let i = 0; i < 16; i++) { fns.push(() => i); } return fns; } var fns = makeFns(); var n = 0; for (var j = 0; j < 100_000; j++) { n += fns[j & 15](); } return n; }
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
console.log("per-iteration " + ((t1 - t0) / 10).toFixed(2) + "ms ok=true result=" + res);
