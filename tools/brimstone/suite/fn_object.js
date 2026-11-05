var t0 = Date.now();
function bench() {
  var objs = [];
  for (var i = 0; i < 1000; i++) objs.push({ a: i, b: i, c: i });
  var s = 0;
  for (var j = 0; j < 1000; j++) {
    for (var i = 0; i < 1000; i++) {
      var o = objs[i];
      o.a = o.a + 1;
      s += o.b + o.c;
    }
  }
  return s;
}
var v = bench();
console.log('fn_object ' + (Date.now() - t0) + ' ' + (v > 0));
