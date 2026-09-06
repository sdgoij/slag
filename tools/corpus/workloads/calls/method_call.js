// Method calls on a shared object (this + own-method dispatch).
function bench() {
  var counter = {
    value: 0,
    inc: function (x) {
      this.value += x;
      return this.value;
    },
  };
  var s = 0;
  for (var i = 0; i < 2000000; i++) {
    s += counter.inc(i);
  }
  return s;
}
bench();
