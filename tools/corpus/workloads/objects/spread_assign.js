// Object-literal + spread construction churn (alloc + property define).
function bench() {
  var s = 0;
  for (var i = 0; i < 200000; i++) {
    var o = { x: i, y: i + 1, z: i + 2 };
    var p = Object.assign({}, o);
    p.w = i;
    s += p.x + p.y + p.w;
  }
  return s;
}
bench();
