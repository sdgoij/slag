var s = 0;
for (var i = 0; i < 10000000; i++) {
  var k = i & 1023;
  s = (s + k) | 0;
}
