var t0 = Date.now();
function sieve(n) {
  var a = new Array(n + 1);
  for (var i = 0; i <= n; i++) a[i] = true;
  var count = 0;
  for (var i = 2; i <= n; i++) {
    if (a[i]) {
      count++;
      for (var j = i + i; j <= n; j += i) a[j] = false;
    }
  }
  return count;
}
var v = sieve(1000000);
console.log('sieve ' + (Date.now() - t0) + ' ' + v);
