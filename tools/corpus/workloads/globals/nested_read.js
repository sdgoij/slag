// The same loop and the same read as `object_read.js`, but in a helper that
// CAPTURES a binding from the function it is nested in — the shape a mod's
// kernels have, where the body's [[Environment]] is the wrapper's env rather
// than the global record. (A nested body that captures nothing is handed the
// global env and was never affected.)
//
// The `LoadIdent` cell probe used to be refused for every such body — the gate
// required the chain to be *exactly* the global record — so each read paid the
// full resolve. It is now per name: a chain that cannot shadow the name still
// serves the cell.
//
// Pair with `object_read.js` (the same read one level up, no capture),
// `declarative_read.js` and `hoisted_local.js`.
globalThis.K = 3;

function bench() {
  var bias = 0; // captured by `kernel`, so `kernel` really has a chain
  function kernel() {
    var s = bias;
    for (var i = 0; i < 1000000; i++) {
      s += i * 3 + K;
    }
    return s;
  }
  return kernel();
}
bench();
