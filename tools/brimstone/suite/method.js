var t0 = Date.now();
function Point(x) { this.x = x; }
Point.prototype.get = function () { return this.x; };
var p = new Point(7);
var s = 0;
for (var i = 0; i < 2000000; i++) s += p.get();
console.log('method ' + (Date.now() - t0) + ' ' + (s > 0));
