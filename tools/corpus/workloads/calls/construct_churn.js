// Construct churn: fresh instances with prototype methods.
function bench() {
  function Item(x) {
    this.x = x;
    this.y = x + 1;
  }
  Item.prototype.sum = function () {
    return this.x + this.y;
  };
  var s = 0;
  for (var i = 0; i < 500000; i++) {
    s += new Item(i).sum();
  }
  return s;
}
bench();
