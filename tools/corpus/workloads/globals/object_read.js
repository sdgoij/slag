// The same read as `declarative_read.js`, but the name is a global OBJECT-record
// data property instead of a top-level `const`: `load_ident` warms the JIT's
// global-value cell for this shape, so the compiled loop serves it as a native
// load. This is the fast side of the pair.
globalThis.K = 3;

function bench() {
  var s = 0;
  for (var i = 0; i < 1000000; i++) {
    s += i * 3 + K;
  }
  return s;
}
bench();
