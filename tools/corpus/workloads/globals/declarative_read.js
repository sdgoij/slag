// A read of a script-level `const` inside a hot loop body. The binding lives in
// the global env's DECLARATIVE record, which is the shape every top-level
// `const`/`let`/`class` in a bundle has -- and, in slag, the JIT's global-value
// cell is deliberately never warmed for one (see `load_ident` in
// crates/runtime/src/jit.rs), so each read takes the full resolve path while the
// equivalent object-record read (`object_read.js`) is a native load.
//
// Pair with `object_read.js` and `hoisted_local.js`: same loop, same result,
// three binding kinds.
const K = 3;

function bench() {
  var s = 0;
  for (var i = 0; i < 1000000; i++) {
    s += i * 3 + K;
  }
  return s;
}
bench();
