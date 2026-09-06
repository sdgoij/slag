// Per-iteration try/catch (no throw) — the handler-frame machinery.
function bench() {
  var o = { x: 1 };
  var s = 0;
  for (var i = 0; i < 1500000; i++) {
    try {
      s += o.x;
    } catch (e) {
      s += 1;
    }
  }
  return s;
}
bench();
